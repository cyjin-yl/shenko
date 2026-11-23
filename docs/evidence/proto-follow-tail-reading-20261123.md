# Reading-journey focus fixture follow-through — 2026-11-23

The original four Inspect/IM reading journeys now pass all **82 checks** against the same twelve production assets at `03b80d770357fb8e7694c0fdac7a2c79911e970c`. The change supplies ordinary Markdown links in the synthetic message fixture and adds a read-only check that historical messages actually contain sequential focus targets. Product code and assets are unchanged.

## Failure and correction

The first run passed Inspect at both widths but failed IM after the original 96 Shift+Tab budget. A separate retained diagnostic run showed that Inspect had 62 sequential targets across 31 message rows, while IM had **zero**. Plain message paragraphs did not exercise the intended historical-link keyboard path in IM.

The fixture now includes ordinary `[Reading source n](https://example.test/reading/n)` text. The unchanged production Markdown renderer creates the native links. No DOM element, tabindex, focus handler or scroll behavior is injected. The additional enumeration only reads visible enabled focus targets and fails unless at least three distinct message keys are represented.

The original 96 real Shift+Tab budget, following real Tab, more-than-100px departure condition, focused-element identity, anchor key, **2 CSS px** tolerance and all waits remain. The existing programmatic composer/resume focus, DOM Range selection and retired-observer replay remain explicitly identified controls.

## Actual verification

- Original run: Inspect 2 pass, IM 2 fail. Diagnostic run: the same two IM prerequisites fail. Both original sets of screenshots, logs and receipts are retained.
- Corrected run: Inspect and IM at **1280×900 and 390×900**, four journeys, 82 passed checks, zero failed checks; elapsed 79.106 seconds.
- The eight keyboard append/anchor checks preserve the same anchor key and focused element; maximum absolute measured offset change is **0.25 CSS px**.
- Runner SHA-256: `520674696a94cafa95023b605b96b109d9dd41e654be36ea26d5b6d60cdd49d4`.
- Successful browser receipt SHA-256: `0164a27f92498d3082b530e891c6c0e5369c6777c912377db6fad220e6d67504`.
- Execution receipt SHA-256: `25cd1165c3d23db118022c1f59266fac69ebcd591809aa60e92ee4c5e942d3f4`.
- All twelve production assets match their Git objects and all three execution receipts. Their previous independent build used Node 24.22.1; this browser runner used Node 24.22.3 and sandboxed Chromium 145.0.7632.6. This invocation did not rebuild those assets.
- Independent source review checked the actual renderer, both callers, all three diff hunks, original assertions and asset identities and found no blocking finding. This is not a platform review approval.

The complete three-attempt evidence and source preimages are retained under [the evidence directory](./proto-follow-tail-reading-20261123/), with a SHA-256 inventory and independent review. The four original final screenshots were visually inspected.

## Visual findings and limits

The mixed Chinese/English bodies, source links, narrow wrapping, composer and Go To Latest controls are visible. The Linux capture renders the fixture emoji as a missing-glyph box. The desktop IM final frame also places the floating Go To Latest control over the lower final-message-line area. These are retained visual findings; this fixture-only change does not claim complete font coverage or full unobscured-reading acceptance.

The four images are final frames rather than a complete adjacent-state sequence. The passing receipt establishes the stated headless Chromium fixture scope in light theme with reduced motion and fixture HTTP/WS data. It does not establish physical-device input, assistive technology, backend/model acceptance, unseen streaming/media/zoom paths, all web-interface §21.11 criteria, or all Shenko S1–S5 work.

Implemented and reviewed with Codex under the user's authorized Sprint continuation. The original failures remain failures.
