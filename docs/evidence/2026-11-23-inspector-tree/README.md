# Inspector: two independent public Tree domains

This increment implements Proto-UI #523 in the existing Shenko application. Tasks use explicit `parent_task_id` facts. Playground artifacts use authored MIME categories; an artifact's `parent_revision` remains revision provenance and never becomes a Tree parent ID.

The increment is based on Shenko `d5894753586ae557e62193ac5a94fb96285601aa`, the Inspector Resizable change published as draft [#402](https://github.com/cyjin-yl/shenko/pull/402). It consumes the explicitly pinned Proto-UI candidate `6a9b575fbe744ccb2f54d1e45336ccc195f401f1`. It does not rebind these results to a later main or candidate.

## What changed

- The actual Tasks and Playground Inspector lenses compose separate public Shadcn Tree Root/Item/Group domains through the existing React adapter. The compiler still generates only Shenko's existing transport badge and message bubble; the browser does not import the compiler.
- Public Tree owns hierarchy validation, disclosure, roving focus, keyboard navigation, typeahead and accessibility relationships. The App supplies records and responds to public activation proposals. It keeps a controlled-empty selected value rather than asserting a new portable single-selection contract.
- A task activation displays the actual task record. An HTML artifact activation enters the existing WorkFrame/adopted-page route. A missing adopted page remains missing; no replacement preview or editor is introduced.
- Subject/source/surface identity replaces each Tree owner. Retired callbacks and a pointer contact begun on a previous subject cannot inspect a record in the new owner. The main stream and editor host retain their identities.
- Duplicate keys, cycles, orphans and empty keys remain authored for public Tree validation. React reconciliation keys for invalid records do not repair their public keys. Invalid domains expose the public invalid state and leave the other domain usable.
- The standalone demo now authors explicit task parent facts. Live task names, roles, team labels and creation order are never used to invent ancestry.

## Exact validation

The machine-readable `validation.json` and `manifest.json` bind every source, asset, observer and raw receipt below. All archived payloads were decoded and verified against their individual SHA-256 values.

| Lane | Actual result | Scope |
| --- | --- | --- |
| Existing public package producer | 42/42, unchanged exact 6a9 source | Reuses the prior source-bound producer, with fresh clean-head/lock verification in both App builds |
| App build 01 | strict TypeScript exit 0; 59/59 Node tests; normal producer exit 0 | Original product candidate |
| App build 02 | strict TypeScript exit 0; 59/59 Node tests; normal producer exit 0 | Current product bytes; source stable |
| Native 01 | 0/4 | Preserved new observer failure: record attributes were read from an adapter host that does not forward arbitrary data attributes |
| Native 02 | 4/4; 63 checks | Preserved success for the Tree flows; subsequent visual inspection found an unimplemented background conversation read fixture, visible as HTTP 404 in the desktop screenshot |
| Native 03 | 4/4; 67 checks; zero page/cleanup errors and business writes; source and assets stable | Same product and original Tree assertions/budgets; explicit `/api/conversations` empty DTO plus a background-read health check in each of the four journeys |

Native 01's original component and observer bytes are retained. The fix places the record marker on the App's existing native label span, and the observer reads the real parent treeitem. The later fixture correction supplies the same `{available: true, messages: []}` read protocol already used by the repository's original reading journey. No product error message is hidden and no tested Tree condition is relaxed.

The four journeys exercise:

1. Desktop Tasks: exact server ancestry, sibling ordinals, Item/Group relationships, pointer disclosure, Arrow/Home/End/typeahead/Enter navigation, task details and domain isolation.
2. Desktop Playground: exact MIME grouping, independent task state, keyboard activation of an actual HTML artifact, the existing missing-adoption path, and stable main/editor identities.
3. A 390px dark forced-colors viewport: visible 2px keyboard outline, actual HTTP/WebSocket invalid-to-valid refresh, all invalid records retained, exact recovered hierarchy, independent-domain recovery and no horizontal page overflow.
4. Subject retirement: a trusted pointer contact begins on the old subject, an actual identity read replaces both domains before release, the late release cannot act on the new owner, and fresh activation still works.

The observer requests `chromiumSandbox: true`, adds no fallback launch flags, and uses the pinned Chrome 145.0.7632.6 executable. Playwright keyboard and pointer events are recorded with `isTrusted=true`; this is browser automation evidence, not physical-device input. The process wrapper stays at 180 seconds and the original interaction polls stay at 1000ms.

The 63 checks in Native 02 and the 67 checks in Native 03 belong to these four Tree journeys. They are **not** the historical 63-row Agent Harness denominator. This increment adds four unit tests and four native journeys, within Shenko's fewer-than-ten-test rule for a coherent increment.

## Reproduction

Use a clean checkout of the recorded Shenko increment and the exact Proto-UI pin in `crates/shenko-ui/web/upstream-pin.json`. Build the ordinary public adapter-react/prototypes-shadcn/CLI closure with Proto-UI's `scripts/build/public-packages.mjs`; keep its actual JSON receipt. From `crates/shenko-ui/web`, run:

```sh
node build.mjs --prototypes-only
node node_modules/typescript/bin/tsc --noEmit
node --test tests/*.test.mjs
node build.mjs
node scripts/tree-browser.mjs
```

The ordinary producer and observer require the source-bound environment recorded in the archived wrappers: `PROTO_UI_SOURCE`, `PROTO_UI_BUILD_RECEIPT`, `PUI_TREE_BUILD_RECEIPT`, `PUI_CHROME_EXECUTABLE`, `PUI_PLAYWRIGHT_MODULE` and an owned `PUI_EVIDENCE_DIR`. The observer independently compares the current source snapshot, normal build receipt, compiler generation manifest, upstream pin/lock and every generated asset before launching.

`inspector-tree-evidence.tar.gz` contains original preparation/build/native scripts and stdout/stderr, failures, current and prior source snapshots, normal generated assets, public producer receipts and all four native screenshots. Native 01's source and receipts are fully reconstructible; its overwritten build-01 static assets are not claimed to be archived. Native 02/03 share the archived normal build-02 product assets.

The relevant React review checked public subpath imports, memoized derived record/presentation data, lazy expansion initialization, event-scoped side effects, current callback references, subject-bound ownership and cleanup. It introduced no raw input/focus state machine or unrelated optimization.

## Boundaries that remain open

This run combines the production App renderer/controller with deterministic HTTP and WebSocket **read fixtures**. It does not validate a live backend/model, physical hardware, platform assistive technology, lazy-loading or virtualization. It does not approve the complete portable Tree contract or resolve its remaining draft alignment.

The preceding Inspector Resizable evidence remains pinned separately. The existing narrow overlay is not promoted here to mobile-modal acceptance. Original reading/follow-tail journeys, the full Agent Harness 63-row ledger, editor fallback boundaries, Apple/120Hz/AT and other Sprint conditions retain their own gates and evidence.
