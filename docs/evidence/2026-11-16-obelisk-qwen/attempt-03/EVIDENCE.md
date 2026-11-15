# Qwen3-4B paired smoke: real native execution, incomplete answers

The real runner ended naturally with **exit code 1** at **2026-11-15T18:09:27.356219+00:00**, after **731,528.557 ms**. Its original report preserves `status=inconclusive_or_failed`, `complete=false`, and `cleanup_ok=true`; both arms are `failed`. Neither arm produced a final reply or reached the judge. The run was not interrupted by the later operator service stop.

This invocation does establish actual native execution: **three baseline `history.query` tasks and two candidate `history.external.query` tasks completed with durable result/receipt chains**. Their returned content did not contain the fixture answer. A successful native operation is not sufficient to establish relevant retrieval or an effective answer.

## Identities and actual server settings

| Item | Retained identity or setting |
|---|---|
| Shenko source | `012370b76735daaa97623bbea4401313b3d9be06` |
| Runner ELF SHA256 | `f043116ebbb25f98bda4a4619474e1e2b22a9b9e2ff2411052cecc394f985fdb` |
| Runner ELF size / execution | 79,397,480 bytes; verified open descriptor executed through `/proc/self/fd` |
| Obelisk source | `cc75c2dd1aead4f4374758a7536385307934e177` |
| Model | Official `Qwen/Qwen3-4B-GGUF` |
| Model revision | `bc640142c66e1fdd12af0bd68f40445458f3869b` |
| File / quantization | `Qwen3-4B-Q4_K_M.gguf`, Q4_K_M, 2,497,280,256 bytes |
| Model SHA256 | `7485fe6f11af29433bc51cab58009521f205840f5b4ae3a32fa7f92e8534fdf5` |
| Server | llama.cpp release `b12359`, commit `b1b770d90d94cad9e9ea5b993bdbc383a522fb28` |
| Server archive SHA256 | `0bc1f091e8916102b6d8f2cf944233a620e90a0f856c72d0ce41150c65c2e72e` |
| Local addresses | Model `127.0.0.1:18834`; provider relay `127.0.0.1:18835` |
| Compute | CPU, 4 prompt/decode threads, 2 HTTP threads, one model slot, no GPU |
| Context | 16,384, explicitly bound to both arm profiles and checked against actual model metadata |
| Memory options | q8_0 K/V cache, flash attention on, cache RAM 64 MiB, no repack |
| Generation | Reasoning off, `enable_thinking=false`, no context shift |
| Sampling defaults | temperature 0.7, top_p 0.8, top_k 20, min_p 0, presence penalty 1.5 |
| Actual subject request budget | 4,096 on every recorded subject POST; no sampling or seed override in those requests |
| Time limits | Product provider 180 seconds; remote curl one attempt with maximum 175 seconds |
| Corpus digest | `blake3:3496021d8a26422e06e775109097c88416786335c57c6fb9c583b4b7d84bb6b8` |

The complete launch scripts, supervisor/resource plan, `--help`, version, model metadata, exact requests and receipts are retained. This uses the same source, ELF, fixture and rubric as the repaired 1.7B round. Model size, weight quantization, K/V quantization and resource settings differ, so it is not a single-variable comparison or an equivalent large-model baseline.

## Baseline: isolated first prompt, three real reads, no fixture answer

The starting window is one actual `notes-rollover-compact` Pointer covering seq 1–4. The original four events remain in the baseline Store. The first subject request is:

- Queue `0002-bf5cba7f694e4f729079ab4f1e0a3fff`.
- 8,097 bytes; SHA256 `a683893b9bfb6b9552c94fcbadb08ee5d82cce22a7d654a58d863f6ea9865c13`.
- None of the four complete fixture messages appears in its message contents.
- `src/auth/refresh.ts`, `tests/auth/refresh-race.test.ts`, `SameSite`, `None`, `Lax`, `generation`, `旧响应`, and `最新请求` each appear zero times; the Pointer marker appears once.

The three completed model responses contain only these native controls:

| Queue | Model-selected native input | Actual result |
|---|---|---|
| #0002 | `history.query` search, query `Hydra auth refresh 并发问题`, from_seq 0, limit 20 | Three matches: this run's question at seq 5, its own delegate output at seq 6, and its own tool_call at seq 7. No fixture seq 1–4. |
| #0003 | `history.query` list, event_types `issue, patch` | `status=ok`, `events=[]` |
| #0004 | Same list/filter | A distinct actual task and result, again `status=ok`, `events=[]` |

The query's full literal text does not occur in any of the four original fixture messages. Their actual event types are `user` and `assistant`, not the `issue` and `patch` selected by the model. The search previews exactly match the original Store JSON slices at their recorded Unicode-scalar offsets. Their truncation flags remain visible.

All three native operations have the real operation/attempt/receipt/result chain. The first search result enters step 3 / wire #0004, and remains visible in wire #0005. The first list result enters wire #0005. The last list result has no subsequent provider visibility in retained evidence. The first search result is present in a committed input step, but its actual event disposition is `ignored` with reason `主体未在本轮处置`; visibility is not counted as effective use.

The final baseline model attempt #0005 times out. Baseline has three committed steps containing delegate actions and one retained `awaiting_model` step. It has no committed reply, final `acceptance.json`, or judge invocation.

## Candidate: two real Obelisk CLI queries, both with empty hits

Queues #0006 and #0007 each emit this actual control:

```text
[delegate:history.external.query:1] {"op":"obelisk","request":{"op":"search","query":"Hydra auth refresh 并发问题","limit":20}}
```

Both outputs are accepted and produce separate committed native tasks. The frozen requests additionally record actual defaults including `literal_fallback=true` and `include_inactive=false`. Each launches the pinned Obelisk CLI through the configured Node executable; the real process receipts record `exit_code=0`, `readers_drained=true`, `readers_joined=true`, and `reaped=true`.

Both results contain:

```json
{
  "full_original_coverage": false,
  "hits": [],
  "mode": "literal_index_fallback",
  "searched_projection": "obelisk_index"
}
```

The coverage receipts identify the configured synthetic Codex root, two observed source sessions, `scope=node_local_snapshot`, `source_text_may_be_truncated=true`, and no complete-original-archive attestation. Empty hits are not evidence that the source files contained no history. Both source JSONL files and all four messages are retained, and their message texts exactly match the baseline's four raw fixture messages.

The first candidate task result is included in the complete JSON presented in #0008. That continuation times out before a model result or committed T2 is obtained. The second result has no subsequent provider visibility. Candidate has two committed delegate steps and one retained `awaiting_model` step, with no committed reply, final `acceptance.json`, or judge.

The compat evidence has two deliberate layers: `task_result.evidence_ref` is the raw observation, while `task_result.result` equals the receipt's normalized CAS object. Its nested `observation` equals the raw observation and `source_receipt` points back to it. The analysis checks these relationships; it does not incorrectly require the normalized wrapper to equal its raw nested observation.

## End-to-end response and native evidence binding

There are eight relay queue entries: one actual `/v1/models` GET, five successful subject responses, and two timed-out subject requests. No judge requests occurred. Every successful origin response is matched to the precise Store subject attempt using byte-identical CAS `response.raw_response` and SHA256. Its `SubjectModelOutcome.raw_response` is independently checked against the same raw body.

Every subject request, including each timeout, also matches the unique CAS input-plan payload/binding serialization byte for byte. Failed results retain `response=null` and unknown usage; their empty HTTP bodies are not treated as model answers. Each native result is linked through task/intent IDs, the originating committed StepDecision, operation frozen input, armed native attempt, decoded evidence receipt, result commit, later input-plan event, original provider request and actual event disposition. Identical empty observations are not used to deduplicate distinct calls.

All five native receipt chains pass the recorded ID/commit and normalized-CAS checks. There are **zero control-protocol rejection receipts** in this invocation. These observations establish actual mechanism execution while preserving the lack of a completed historical answer.

## Timeouts, context and measured usage

| Scope | Subject POST attempts | Complete model responses | Observed prompt tokens | Observed completion tokens | Observed total | Cached prompt tokens |
|---|---:|---:|---:|---:|---:|---:|
| Baseline | 4 | 3 | 7,983 | 77 | 8,060 | 3,982 |
| Candidate | 3 | 2 | 4,965 | 76 | 5,041 | 3,831 |
| Total | 7 | 5 | 12,948 | 153 | 13,101 | 7,813 |

These are **observed subtotals**, not complete run usage. #0005 and #0008 have unknown actual input/output usage. Their template/tokenize checks measure capacity only and are not substituted for provider usage.

Both failed requests retain remote curl exit 28, HTTP status 0, and empty headers/body. #0005 has a local `caller_disconnected` delivery receipt with relay elapsed 181,678.074 ms. It does not prove that the caller received the relay's generated error status.

**#0008 has no `delivery.json` or delivery event.** Its remote attempt ended at 18:09:26.318681 with an elapsed attempt phase of 175,018.425 ms. Its exact response transfer was verified locally at 18:09:29.539834, after the runner had already exited. Local delivered status and relay elapsed time therefore remain null; no HTTP502 delivery is invented.

All seven original requests have a successful pre-inference capacity check within 16,384 tokens. The maximum is #0008: 4,995 prompt tokens plus 4,096 requested output tokens = 9,091. Each successful response's actual prompt usage matches its preflight count.

The retained server log shows long prompt processing and subsequent cancellation for the unique 4,877-token request (server task 93, baseline #0005) and 4,995-token request (server task 178, candidate #0008). It does not provide a successful final HTTP response or usage receipt for either, and the analysis does not manufacture token totals from partial log counters.

These timeout boundaries belong to this actual relay setup: the remote helper cancels its single curl attempt at 175 seconds, while the product provider has a 180-second deadline that also spans relay overhead. Queueing, preflight, transfer and whole-body buffering add time. Baseline #0004 already took about 172.9 seconds in the remote attempt phase and 179.398 seconds through the relay. Therefore the two timeout observations must not be described as a measured failure of a direct, unrelayed provider with a native 180-second budget. No such separate experiment occurred.

The five successful remote attempt phases total 334,555.355 ms. Including both timeout attempt phases gives 684,599.334 ms. Recorded relay elapsed time totals 551,401.081 ms **over only six POSTs with delivery receipts**; candidate's last relay duration is unknown. Runner elapsed time is 731,528.557 ms. These different scopes must not be compared as equal totals. Connector transfer, queueing, polling, preflight, buffering and prefix-cache state affect timing; this is not a native latency or token-saving benchmark.

Prefix cache was not reset between arms or requests. Reported `cached_tokens` and differences in warm-cache state are retained as observations and are not counted as retrieval benefits or token savings.

## Model service remained alive; later owned stop was cleanup

At 18:10:11, after the runner failed, the server still existed and returned health HTTP200. The guard had not triggered. The supervisor retained 512 resource samples; independently recomputed sampled maxima/minima are:

- Maximum model RSS: **3,782,740 KiB**.
- Minimum system MemAvailable: **2,044,828 KiB**.
- Swap: zero; no resource-stop record.

These are sampled observations, not a guarantee about every instant. They provide no support for attributing this run to a model-service disappearance or an OOM event.

The root-authorized cleanup sends SIGTERM only to the verified owned server PID 49309 at 18:10:53.375886, approximately 86 seconds after runner completion. The supervisor records graceful return code 0 at 18:10:54.380335. A later observation at 18:11:36 reports both server and supervisor absent and no guard event. This later stop did not cause the runner failure and is retained separately from request cancellations.

The runner's broker shutdown records local admission closed, local provider readers drained, `remote_termination_claimed=false`, and both final requests `state=unknown`. Thus `cleanup_ok=true` remains the original local runner cleanup result; it is not used to rewrite unknown provider outcomes.

## Review entry points

- `analysis/result-summary.json`: exact identities, per-wire hashes/usage/status/timing, every subject T1/E1/T2 trace, native counts and original runner status.
- `analysis/native-retrievals.json`: complete actual native inputs/results, operation and receipt IDs, raw/normalized CAS relationships, returned source content, wire visibility and dispositions.
- `analysis/retrieval-support-review.json`: fixture/source equality, model-selected query/filter checks and the limits of the retrieved content.
- `analysis/baseline-prompt-isolation.json`: first-wire answer isolation and actual Pointer state.
- `analysis/db-audit.json`: original Store column values and schemas in a read-only export; derived FTS tables are counted but omitted from the text export.
- `analysis/cas-index.json`, `cas-reference-verification.json`, and `cas-export-map.json`: all **55 CAS originals**, SHA256/length checks and exact selected exports. BLAKE3 references are retained; the analyzer does not claim to have independently recomputed BLAKE3.
- `relay/queue/`: all original provider requests, origin response bytes/headers, remote transfer receipts, and the observed presence or absence of delivery receipts.
- `model-service/final/`: complete transferred server/request logs, template/tokenize capacity artifacts, resources, stop/exit observations, and remote-transfer verification.
- `run-identity.json`, `build-binding/`, launch/relay/supervisor scripts, both starting contexts, original `report.json` and `broker-shutdown.json`: reproducibility and identity evidence.

The full archive retains the original databases with sidecars, native index files, all original CAS objects and the explicitly provisional analysis snapshots. Compact mirrors portable relative paths, keeps all raw CAS/wire/text evidence and final analyses, and lists omitted database/binary/snapshot files in `INDEX.json`. `CAS-LOCATORS.json` maps unchanged absolute CAS locators to portable paths. Model weights, the runner ELF and server binaries are identified by hashes but not included.

No product prompts, original requests, outputs, retrieval data, judge judgments or strict gates were changed. This run supports the reality of the native history and external CLI execution paths. It supplies no completed model answer, judge score, retrieval efficacy or token-saving evidence, and does not complete large-model or repeated-scenario acceptance work.
