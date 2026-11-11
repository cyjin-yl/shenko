# `document.parse` — OXR provider evidence (2026-11-11)

Issue #89 `document.parse` v1. This record states what the `OxrProvider`
slice actually does, what it proves, and what it cannot prove on this host.
It is a mechanism record, not a claim that a real OXR engine ran.

## Upstream interface shape (as documented)

The provider implements the SoMarkAI/OXR HTTP contract exactly as upstream
documents it (`docs.somark.ai/en/api-reference` + the "API alignment" page +
`SoMarkAI/OXR` README):

- **Transport** — the engine is an HTTP *service*; there is no in-process
  API.
  - `POST {base}/parse/sync` — one multipart upload, whole result in one
    response.
  - `POST {base}/parse/async` — submit, returns `task_id`, then poll
    `POST {base}/parse/async_check` every 3–5 s until `status` is
    `queuing` → `processing` → `success` | `failed`. OXR keeps async tasks in
    server memory; a restart loses them (→ unknown outcome, not a failure).
  - Base URL: `http://<host>:8000/v1` for self-hosted OXR,
    `https://free-oxr.somark.ai/api/v1` for the hosted free API. A base with
    no path is normalized to `/v1`.
- **Envelope** — `{ code, message, warnings, data }`, `code == 0` on
  success. Per-page data at `data.result.outputs.json.pages[]`:
  `page_num` (0-based), `blocks[]`, `page_size { w, h }`. Per block: `idx`,
  `type`, `bbox` (`[x1, y1, x2, y2]`, integers), `content`, `format`,
  `captions[]`, `img_url`, optional `title_level`, cross-page refs.
- **Credential** — `api_key` is a multipart *form field*, not a header and
  not a URL component. Self-hosted OXR requires the field to be present but
  does not validate it (`dummy` works); the hosted product validates it.
  `CredentialHandle` keeps the secret out of every log/error rendering.
- **Limits** — hosted SoMark: QPS 4/account, 200MB / 300 pages. Free OXR
  API: 10MB / 20 pages, queued. Self-hosted OXR: resource-dependent. The
  boundary's default page concurrency is the provider's declared 4, capped
  at a hard 16 regardless of configuration.
- **Element types** — the brief said "19". The public upstream docs do not
  fix a single count: the OXR README enumerates 13 JSON `type` values with
  OXR-native spellings (`Page-header`, `Page-footer`, `Picture`,
  `Chemical Equation`, `Code Block`, `Chemical Structure`, `Caption`,
  `Footnote`, `Formula`, `Text`, `Title`, `Table`, `Stamp`); the hosted
  SoMark "Recognized Document Elements" gallery shows 21 icons. The spellings
  differ between the OXR-native output and the SoMark-compatible API. The
  `PageBlock.element_type` field therefore carries the upstream token
  **verbatim** — no allowlist, no denylist, no renaming — so a count is never
  baked into the boundary.
- **Content formats** — upstream `format` values include `text`, `latex`,
  `mathml`, `ascii`, `html`, `markdown`, `image`. Carried verbatim;
  absent = unknown, never inferred from the element type.
- **Unsupported options** — OXR rejects `zip` output, `file_url`,
  `element_formats.image=file`, `formula={mathml,ascii}`, `table=image`,
  and the `enable_*` feature flags (a `true` on an unsupported `enable_*`
  does not fail; it is ignored with a `warnings` note). This client sends
  only `file`, `api_key`, `output_formats=json`, so no `warnings` are
  expected — the field is still surfaced on `OxrDocument`.
- **Error codes** — rate limits (`1101`, `1124`) and task-loss/timeouts
  (`1102`, `1120`) map to `PageOutcome::Unknown` (retryable; the request may
  still have been processed). All other non-zero `code`s, plus `failed`
  status, are terminal `PageOutcome::PageFailed`. Non-2xx HTTP: 408/425/429
  and 5xx are unknown outcomes; other 4xx are terminal.

## Local GPU boundary — why no real OXR ran here

This host has **2× Tesla V100 PCIe 32GB (Volta, SM70)**. OXR's documented
runtime requires an NVIDIA GPU with **BF16 tensor cores** — BF16 hardware
arrives at **Ampere (SM80)**. Confirmed on this host:

- no `nvidia-smi`, no `/dev/nvidia*`, no NVIDIA kernel module;
- no `nvcc`, no CUDA runtime, no `torch`;
- therefore **no OXR engine can run here**, and any claim of a real OXR
  parse on this machine would be false.

The `OxrEngineBinding::Local { attestation }` knob exists for the future
when an OXR is colocated: the provider does not probe the GPU itself (it
cannot), it requires the deployer's attestation and reports Unavailable
without one. On this host an unprobed local endpoint is Unavailable because
the BF16/CUDA-13 prerequisite is factually unmet.

## What this slice proves

- `OxrProvider` is a real HTTP client, not a shell: endpoint + model
  identity + scope + credential bound; the upstream response (pages /
  blocks / bboxes / page_size / element type / format / captions) maps into
  `PageResult`/`PageBlock` verbatim, missing fields → `None` (never
  fabricated).
- `PageBlock` gained `element_type` + `content_format`; `PageResult` gained
  `page_size`; `PageCheckpoint` gained `unknown_outcome` (unknown effect,
  distinct from terminal failure). Old serialized checkpoints still
  deserialize (additive `#[serde(default)]` fields only).
- `DocumentProvider` is async; `run_parse` runs pages under a bounded
  concurrency (default = provider's 4, hard cap 16), one durable checkpoint
  per page, in page order, which the caller seals — this crate does not
  touch the Store.
- Honest availability: no endpoint / no model identity / no credential /
  invalid URL / local engine without attestation / unreachable endpoint all
  return `ProviderUnavailable` naming exactly the missing prerequisite —
  never a parse, never a substitute.
- Per-document single-flight: `parse_page` calls on one source share one
  upstream upload (upstream has no per-page endpoint), so the page count
  never multiplies QPS.

## What this slice does NOT prove

- **Real OXR recognition quality** — no real engine ran here; nothing
  upstream-shaped was asserted about accuracy.
- The free/hosted API's *credential acceptance*, *rate-limit behavior under
  load*, or *page-count beyond 20* — those need a reachable upstream.
- Async-task behavior across a server restart (tasks are in-memory
  upstream) — coded as unknown-outcome but not exercised against a real
  restart.

## Verification run

`cargo test -p shenko-document` — the new `oxr_client_tests` exercise the
real HTTP path against a local mock speaking the upstream shape (sync and
async-by-task_id), the old-payload deserialization, the concurrency bound,
the unknown-vs-terminal distinction, and every availability reason. The
existing `tests.rs` structural tests were migrated to the async API
(semantic assertions unchanged — the `OxrProvider` constructor changed
from `(engine_path, model, scope)` to `OxrProviderConfig` and `availability`
became async, so the call sites were updated; every assertion keeps its
meaning).
