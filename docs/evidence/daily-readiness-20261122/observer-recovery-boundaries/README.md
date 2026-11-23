# Recovery observation boundaries in the original c931/001 journey

The original first attempt of `final-c931526c-observer-e537896f` finished at
2026-11-22 01:09:45 UTC after 932.352553384 seconds. Its original result is
**failed**, with exactly two false checks: `replayed_input_same_event` and
`browser_existing_messages_retained_once_at_finish`. This diagnosis does not
change that verdict or count a new successful journey.

The binary source is `c931526c8ae8cd16f3e0cf56d733f1e4275d5bb0`; the frozen
observer source is `e537896f771233ea2b8d01758b3acb86fb0c6345`. The attempt had
already completed both real child results, a causally bound first answer,
durable pause, clean process restart, and a causally bound continued answer.
Its original effect/result/receipt comparisons and shutdown checks were true.
Independent semantic review and the full original batch remain separate from
these two observer diagnoses.

## The actual replay receipt and the mixed observation list

Line 127 of the original `restart.websocket.jsonl` is the receipt actually
received by the new observer socket for its explicit replay. It has
`stage:accepted`, `replayed:true`, original event
`cc49aa42-b03e-4e20-a001-7f248920dbb7` and original accepted commit
`2dc58d06-c49d-4dc0-b75a-f9a3442e3f7f/11`.

The page's retained history also includes its original receipt at line 7 of
`browser/events.jsonl`, with the same identity and `replayed:false`.
`admit_then_resume` appended that full page history to its observation list;
the later first-match lookup could select the old receipt before the actual
new-socket receipt. The complete original combined Python array was not
retained. The decoded history-first projection in [diagnosis.json](diagnosis.json)
demonstrates this selection problem using the original captured rows; it is
explicitly a projection, not an invented original trace.

Fix `ba0cc20b1b0867a55ab367fb70817783026da673` retains a separate copy of the
current socket's received rows before appending page history. The replay
lookup uses only that same socket's admission and continuation receive
windows. It selects an accepted receipt and retains the selected original
row in the summary. The original event, accepted commit and `replayed:true`
conditions remain required. No extra submission, model call, receive window
or retry is introduced; the protocol-only caller adopts the same return
shape. Page history cannot supply a previous replay as proof of this one.

## The view had changed before compiled bubble content was rendered

After Inspect returned to the conversation, `journey-finished.dom.json`
contained five unique message keys with empty rendered text. Its subsequent
screenshot already showed the real conversation. The next `final.dom.json`
contains the same five unique keys with their rendered text; both original
stable facts match their before-restart text exactly. The root and independent
reviewer inspected the original captures. The same unmodified
`stable_messages_match` method returns false for the premature DOM and true
for the later DOM. No original capture is replaced.

The fix waits for the original key/count/text predicate after that view
change, within the witness's existing 15-second UI-operation limit. It
records readiness and elapsed time, then captures the live DOM and applies
the original strict predicate again. Persistent loss, duplication or changed
text still fails, as does a timeout. This observes final rendered retention;
it does not establish flicker-free rendering, a rendering-latency improvement,
or a complete real journey on the fixed observer.

## A visible stream did not prove the page received its committed answer

Independent review found a further gap in an originally **true** check.
The resumed S1 was durably committed at `/908`, and the independent observer
socket received its exact terminal `timeline_part` at line 482. The actual
page's original event capture contains 49 stream parts for that step, but
**no terminal committed frame** for it. Its Inspect capture labels the
answer as uncommitted; its final conversation DOM makes the text visible but
does not mark that row as a stable fact. For the first answer at `/490`, in
contrast, the page recorded exact committed frames at indices 903 and 1124.
These original frames and source-file hashes are retained in the derivation.

The subsequent `c5f37923` observer fix requires a received frame on the
actual page with the selected step, exact S1 commit and full original body,
terminal and committed flags, and the actual base or exact corrected part.
It then waits for the production Inspect entry with those same message,
part and commit attributes and `data-committed="true"`. The original visible
result checks still apply. Frame arrival and DOM readiness each have a
15-second UI-operation boundary; together they can take up to approximately
30 seconds. The actual page socket and frame index are retained.

This closes a false-positive opportunity in the existing named committed
output check. It does not claim the original page received that commit, that
its delivery necessarily exceeds 15 seconds, or that product latency was
fixed. The original page was closed shortly after the independent observer
noticed S1. The next real run must wait for actual page delivery and
presentation before cleanup; a missing or late projection remains failure.

## Reproduction and validation scope

[derive.py](derive.py) reads only the original closed attempt's captures and
records all ten input files' byte lengths and SHA256 values. It uses the
actual observer receipt selector, browser retention predicate and committed
page-frame predicate. Given an
extracted original `public/attempt-001` directory:

```sh
python3 docs/evidence/daily-readiness-20261122/observer-recovery-boundaries/derive.py \
  /path/to/public/attempt-001
```

The four existing observer regression functions passed after the source
replay/render-boundary changes (4 passed, 0 failed, 0.232 seconds). No additional test function was
added. Independent source review found no P1/P2 within these changes and
checked all three callers of the changed admission return shape. The
derivation itself ran against the subsequent exact fixed observer source and
records its file hashes. It performs no Store open, model/browser run,
submission replay or mutation of an original receipt. A new fixed-candidate
batch must provide the subsequent end-to-end evidence.
