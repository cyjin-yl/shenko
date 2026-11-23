# Original reading journeys on the Inspector candidate

The repository's existing `scripts/web_reading_journey.mjs` ran unchanged against Shenko commit `7a8e5fc88404ac274e6b93a830e0744d010baebc`, with the explicitly pinned Proto-UI candidate `6a9b575fbe744ccb2f54d1e45336ccc195f401f1`. Its SHA-256 is `520674696a94cafa95023b605b96b109d9dd41e654be36ea26d5b6d60cdd49d4`.

The original four journeys passed: Inspect at 1280 × 900 and 390 × 900 (21 checks each), and the actual conversation view at both widths (20 checks each). That is 82 checks within four existing journeys. It is not the 63-row Harness inventory.

The process ran from 2026-11-23T17:52:51.952279Z to 17:54:02.858398Z and exited 0. The application commit, clean tracked status, source snapshot, original observer and all normal generated assets stayed unchanged. The original raw receipt has SHA-256 `6d7f2f813e1fcd1f9422fa36555bf6d055a60c4576c68c553f7fffcbbcaca629`; the normal build manifest remains `876c52de5fa050bf6d71ebae5b57d607e5adddbed1972b5552be6767e423f9c4`.

## What the original script observes

The production App renderer/controller consumes deterministic HTTP/WebSocket committed-text fixtures and ordinary Markdown links. The original assertions cover actual appended text, following the tail while preserving composer focus, sequential keyboard departure into history, user scroll priority, paused anchors, duplicate replay, keyboard resume, later appends, selection retention, natural end arrival, subject replacement, late ResizeObserver callbacks and actual App disposal. The original scroll/anchor tolerances, focus loops, 10000ms operation timeout, 50 × 100ms end-settle loop and timing waits are unchanged.

Wheel and keyboard input comes from browser automation. Selection uses the original DOM Range setup; pagehide and retained observer delivery are the script's explicit lifecycle controls. This does not claim physical selection/scroll hardware, live backend/model behavior, or assistive-technology acceptance.

The original launch expression remains `chromium.launch({headless: true, ...executablePath})`. The wrapper supplies the pinned executable and Playwright module but adds no launch flag, monkeypatch, sandbox environment override or fallback retry. Its 600-second process envelope contains all four journeys and does not replace or expand the original per-operation gates. The first transport request returned a queued timeout without creating a start marker or process; read-only reconciliation is archived. The successful start used the same creation-only marker, and one execution produced the recorded results.

## Reproduction and archived evidence

On the recorded clean candidate, after its normal source-bound frontend build:

```sh
READING_EVIDENCE_DIR=/owned/output \
PLAYWRIGHT_MODULE=/verified/playwright-core/index.mjs \
CHROMIUM_EXECUTABLE=/verified/chrome \
node scripts/web_reading_journey.mjs
```

`original-reading-evidence.tar.gz` contains the unchanged observer, exact source and built assets, the launch wrapper and reconciliation, stdout/stderr, complete original receipt, all four screenshots and source/asset checks. `manifest.json` lists every archived payload's SHA-256 and byte size; all entries were decoded and verified. The preceding Tree increment and its failures remain in `../2026-11-23-inspector-tree/`.

## Remaining #746 and Harness scope

This is current evidence for the existing committed-text reading composition. #746 additionally calls for deterministic public thought/activity, tool, receipt and explicit-wait updates through the actual application facts and their shared fixtures. Those additional semantic-update scenarios are not silently covered by this text-and-links run. The accepted-consumer/matrix review and the broader 63-row Harness inventory remain independently scoped. No legacy source-bound result is relabeled as a new candidate or a passing live-service run.
