# Independent causal and semantic audit of the original d16 batch

**The original complete journey result remains 0/3.** This audit distinguishes
an actual first-task result, an observer false positive, and a product failure.
It does not replay an installation, execute another model request, change an
original receipt, or award recovery coverage to an unreached phase.

The product source was `d16adbf89c9f79dfce8ef90d77843412bcda85bf`; its release
binary was 90,351,680 bytes, SHA256
`d44703d84ff02a4235005e54c935ddeb2b6dc78a4dbe4184e981c293f1daccce`.
All three used the original browser observer, the reference
`swift-qwen3.8-27b-nvfp4` service at `http://10.10.10.11:8000`, a 4,096-token
main-request budget, 900-second phase observation and 300-second startup
observation. The original deployment/configuration/binary/observer manifests
and failed summaries remain in [the public batch record](../final-d16adbf8/).

| Original attempt | Actual durable and semantic observation | Why the full original journey failed |
|---|---|---|
| 001 | Both T2s applied; the selected request actually contained both complete original results; its S1 accurately reported `437` and `S1-cf299c3653c5`. That report is also visible in the final conversation screenshot. | Inspect navigation used a text locator that matched both a button and its child span. The requested Inspect observation and recovery phase did not complete. |
| 002 | Both T2s applied, but the observer selected an earlier-sealed **waiting** answer. Neither original result was in that request. The model explicitly refused to substitute its own arithmetic for the child output. | The old commit-order/text gate was a false positive; the same Inspect locator then failed. There was no later result-consuming answer or recovery phase. |
| 003 | A truncated first response was retired. Its replacement delegated once. `printf` and the model child's provider step produced real E1s, but no TaskResult/T2 applied. The only S1 said it would await real results. | Direct native settlement failed with `snapshot object closure is incomplete`, then authority recovery failed. Process exit was 1 and the observer lost its connection. |

## What was independently checked

The immutable packet archives preserve the original semantic CAS objects and a
separate read-only export of native proof records from each closed installation:

- [001 packet](attempt-001.tar.gz): 292,793 bytes, SHA256
  `bb64532c74ced9ae8473063d9cbd7f89e7b85751e7d5422746cd6f973964900f`.
- [002 packet](attempt-002.tar.gz): 260,363 bytes, SHA256
  `782cb4286fc58088130756eaaed4383a00798f0e5cb5ce31762643a075579154`.
- [003 packet](attempt-003.tar.gz): 243,818 bytes, SHA256
  `940c79c880a299651fbee5e4a9f8c44a8bb5506c35b6a58638aa9e078cfc653f`.

[observations.json](observations.json) is the derived evidence map. It checks
all 84 retained CAS file copies (67 semantic and 17 native), their byte lengths,
SHA256 values and complete typed references, plus all 26 decoded source-file
identities recorded by the semantic exporter. It keeps all nine main-request
attempts, six committed bodies, four applied TaskResults, three original child
provider responses, eight E1 receipts, and all six durable runtime-error events.
The raw original ledger/browser files are in the separate public batch record;
these packets alone are not a complete installation or a complete raw archive.

For all seven main requests with a provider response, the retained streaming
`delta.content` concatenation equals the stored response content. All three
child responses instead use ordinary `chat.completion` **`message.content`**;
that field equals the recorded child text and its text CAS object. Reasoning
fields remain separate. All nine main request effective bindings match their
original sealed provider bindings, including the two terminal Unknown records.
The child request, frozen step input, original provider response, normalized
step record and actual model binding agree in all three attempts.

The frozen [semantic exporter](export_semantic.py) intentionally remains byte
identical to the one run remotely. Its original convenience field
`exact_full_result_in_actual_payload` did not recognize the production
`[event <id>]` prefix. The separate [derivation](summarize.py) explicitly parses
that prefix after the runtime tool-result header and compares the **complete
original TaskResult JSON**. It also checks the selected plan's actual events
and same-scope history bounded by that plan's `leaf_seq`. It never repairs an
old request using later rows from the final ledger.

## 001: a real result-consuming answer, followed by an observer failure

The first S1, sequence 72, created exactly one model child and one `host.printf`
child. The later waiting and reporting S1s created no additional tasks.
The native `printf` original observation has exit code 0, empty stderr, no
transport error, exact postcondition true, and stdout `S1-cf299c3653c5`.
Its T2 is sequence 89. The finite child's actual provider response is exactly
`437`, terminated with `stop` after 131 output tokens under its 256-token
request limit. Its one-step transcript has no tool calls; the child result's
T2 is sequence 95.

The successful report is bound to request
`23ee7714-19d6-498b-b3f1-69e7076ba9d0`, created at sequence 237, and committed
step `b12cfbda-6959-4161-8571-c50efddda255` at sequence 446. The actual
`subject_step_input` object is
`blake3:82b67c56a0da8379ce62eed56394207bcf1b93c9a5a720492919fbeb982a427d`,
76,270 bytes, SHA256
`89680182d673127da02a3e6419c604dfc71264aa94029042d1a6ea037a0f0486`.
Its payload messages 6 and 7 contain the complete two original TaskResults,
including their task, result, operation, receipt and commit identities. Both
also appear as sealed events and as original history rows 7/8 within leaf 9.
The decision's input reference, active request, step, stored model text and
original provider output agree. The final answer reports the correct values,
source model, one-step/no-tool child behavior, and T2 sequence numbers.

There is one narrower semantic limitation in the preceding waiting answer.
Request `2800223d-7ba6-4fcb-9376-cd58884e938f` was created at sequence 78 and
had neither result. Its S1 at 231 said that seeing no `task_result` meant the
children had not actually returned. That inference goes beyond the sealed
input: the real T2s had already committed at 89/95 before that answer committed.
It did not falsely claim completion or delegate again, but its wording should
have stayed limited to what that request could see. The later report at 446
does have the required actual input and correct result attribution.

The browser's original received `timeline_part` at JSONL line 537 names the
same message/step, `text/0`, `terminal=true`, exact S1 commit `/446` and exact
committed body. The separately inspected [final screenshot](../final-d16adbf8/attempt-001/browser/final.png)
and its DOM/ARIA capture visibly show this report in the default conversation.
The DOM export records the matching part key and rendered text; it does not
independently record DOM `commit_ref` or `data-committed` attributes. The proof
therefore combines **actual visible conversation content** with the separate
original frame/ledger binding. It does not turn the failed Inspect click into
a successful Inspect observation.

Owned shutdown was clean and the process group was reaped and empty. A later
main request `d4838efe-05be-45d3-9b58-4950f9fdb174` had already started; its
original outcome remains Unknown after local shutdown, with remote termination
explicitly unconfirmed. That record is retained, not renamed a failed tool
effect or removed because the earlier report was correct.

## 002: why commit order and answer tokens were insufficient

The first S1 at sequence 192 created the two requested children. The exact
`printf` marker was `S1-08f1c04d2de0`; its T2 applied at sequence 210. The child
provider returned a correct `19×23 = 437` explanation, with `stop`, 176 output
tokens and a 2,048-token actual child request limit. Its transcript/text/raw
provider objects agree and its T2 applied at sequence 218.

The incorrectly selected answer belongs to request
`a640001a-9287-4ce5-8d4f-9ab507fcf9bd`, created at sequence 200, and committed
step `b07e0df2-644c-4539-8634-8afd66c45031` at sequence 335. Its sealed input,
`blake3:525cf271f690b1ca396e7257d1f2eeb685f34cc05320fd309df16482bf28c8d2`,
is 66,805 bytes, SHA256
`bd1686ad69edbc55ff19ae5a421b2418c9369a991e80a0fe278c2e51e2f826d9`.
Its five payload messages contain the original user request, the preceding
delegating answer, two Pending tool calls, and the user request again. There
is no original TaskResult in its actual payload or sealed events. The later
tool-result history rows 7/8 lie beyond its sealed leaf 5.

The committed answer explicitly says that its current context has received no
`task_result`, that the repeated user request is not a child receipt, and that
it will not substitute the `437` in its own mind for the child's output. It
continues waiting. The old observer nevertheless accepted the existence of a
late S1 containing `437` and the marker. The actual result had not been read by
that request; the model did not claim it had. The [final screenshot](../final-d16adbf8/attempt-002/browser/final.png)
visibly shows this waiting answer. Received frame line 432 binds its exact
body and commit `/335`, which proves publication of that waiting body, not
completion of the requested report.

No main request after sequence 200 exists in the final original snapshot, so
there is no later successful report to substitute for the falsely selected
one. Owned shutdown was clean and no runtime-error history row was recorded
in this attempt. The first-phase false positive motivated the subsequent
observer causality gate; it does not alter this attempt's original failure.

## 003: real E1s, unmaterialized T2, and no completed report

Request `8c8d47bb-fb70-4c38-8acb-b2f6bb9c0a70` ended with `length` at
4,096/4,096 output tokens and empty answer-channel content. The runtime's
original log records `MODEL_RESULT_INCOMPLETE` and R0 replacement at
00:03:16.690497 UTC; the retired step has no S1 decision. Its replacement
request `0d77b6c3-e7da-4320-9fbc-9681526b15d0` ended normally at 3,053 tokens.
The sole committed S1, sequence 101, explicitly says it will delegate and wait
for both actual results before reporting success. It creates each requested
child exactly once; it is not a completed-results answer.

The `printf` operation really produced `S1-1255ccfdf2f4` with exit 0, empty
stderr and an exact postcondition; its original E1 is sequence 114. The finite
child provider really returned `437`, `stop`, 55 output tokens under its
256-token request limit, with a matching original response and normalized
step record at E1 sequence 119. These are actual observations, but neither
task has an applied TaskResult in the final snapshot. Both tasks remain
durably Running after the runtime has stopped; no live process is implied by
that durable state.

The closed-store export retains direct T2 preparation
`c9a05d69-2294-f03a-dfdc-f4f1f88a5b2c`, codec 4, planned result
`391e529c-a2c2-4130-c316-3ca8f466eaef` at logical sequence 118. **There is no
matching T2 materialization marker and no corresponding TaskResult row.**
The prepared result is not an applied effect receipt. The original runtime log
at 00:05:08.596498 UTC reports
`invalid direct native predecessor: snapshot object closure is incomplete`.
It then records authority fencing and reconstruction; the terminal log retains
chosen control 15 without a local applied marker. This audit preserves that
boundary. The separate product investigation of why the snapshot closure grew
is not reproduced or inferred solely from these semantic packets.

The next main request, `f78537f5-a9fb-4d4f-946b-1614239f4d98`, remains Unknown.
The original received timeline frame for the delegating S1 has the exact body
and `/101` commit, but the [final screenshot](../final-d16adbf8/attempt-003/browser/final.png)
shows the input, a quiet-thinking indicator and reconnecting status, with no
final report. The parent exited 1; its process group was reaped and empty.

## Native proof and preservation limits

For every attempt the native exporter retained the full typed codec-6 wrapper,
its codec-2 inner E1, the original T1 admission payload and arm transition,
their local materialization markers, and the T2 transitions described above.
Each wrapper's predecessor list exactly matches its inner E1's original arm
payload. The original receipt equals the retained final snapshot. The applied
T1 marker's admission and transition match that same original arm. In 001/002,
both T2 markers and complete TaskResults also agree; in 003, the missing marker
and result remain missing. No old Unknown or failed fact was rewritten.

The [native exporter](export_native.py) ran only after each summary existed,
the explicitly identified owned group was reaped/empty and absent, and the
installation path matched that exact attempt. Each installation was read in
one read-only Store transaction. This is not a joint Store/Node transaction;
the native exporter did not open Node DB. Its before/after records show the
same DB, WAL and SHM byte hashes and sizes. SHM **mtime changed** during each
read-only inspection, and is recorded rather than described as untouched
filesystem metadata.

Each final seal-debt table is empty. That proves only the final observed state,
not that no transient debt ever occurred. Original runtime warnings and durable
error history remain available. This audit did not reread or transport the
entire 90 MB implementation or every predecessor object: it validates the
retained proof links and original raw bytes, not the full closure independently.
BLAKE3 verification is `null` in the exported records; independent checks here
use SHA256 and original reference lengths. Complete typed references are kept,
so same-digest aliases are not silently declared identical.

There is no restart/recall success in these runs and no evidence here of
post-restart nonduplication. The first phases show one `printf` operation,
attempt and original receipt per installation, and one requested model child;
that narrower observation does not substitute for the unreached recovery test.

## Reproduction

After retaining the separate public batch directory beside this directory:

```sh
python3 -I summarize.py > observations.reproduced.json
```

`--originals-dir` may point to the same immutable public originals elsewhere.
The command reads tar members and original files only. It performs no SQLite
access, writes no installation state and makes no network/model request. The
[manifest](manifest.json) binds every file in this audit, including the exact
export scripts, raw packet archives and deterministic derived JSON. Semantic
judgments and the direct inspection of the three PNGs are stated above rather
than disguised as automated model-quality assertions.
