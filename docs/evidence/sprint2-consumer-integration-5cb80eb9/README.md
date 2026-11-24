# Sprint 2: regenerate the existing Inspector consumer at the integrated Proto pin

This is a continuation of the existing Shenko draft #402 source at `2816a6ad`, including the public Resizable Inspector and the independent Tasks and Playground Tree domains. It changes the exact upstream pin and regenerates the ordinary production assets. It does not reimplement those components, replace the existing App, publish a release or change the PR's Draft status.

## Source and distribution boundary

The Proto-UI pin moves from `6a9b575fbe744ccb2f54d1e45336ccc195f401f1` to the frozen Sprint 2 integration candidate `5cb80eb937f20982dd7dc8cbc93a90e91d665c31`. The separate producer checkout was detached at that exact commit and was clean before and after the build. The lock bytes remain `6e21c4b8444ca5e4a2e427287a76a346137b65d76403579942269bfea1fc7fe8`; all three public entry versions remain `0.3.0-alpha.2`.

The original public producer was executed for `@proto.ui/adapter-react`, `@proto.ui/prototypes-shadcn` and `@proto.ui/cli`. Its actual plan selected 42 of 47 public packages. Every name and its order in the successful 42-package result matched that plan. The existing build's 42-package check therefore matches this candidate; no count, receipt or source guard was altered to make it pass. This public producer is separate from any npm pack experiment or another candidate's build.

Resizable and both Tree domains continue through the public Shadcn exports and the existing React adapter. The private compiler still runs only at build time to compile Shenko's own `transport-badge` and `message-bubble`. Its default export remains `./src/index.ts`, with a separate source `./browser` entry; it is not a published compiler package or a browser runtime import. The current physical compiler source-tree digest is `ed12506e8152aa778d12917ab3929cb0cda615e5f763d0076f8383fb3b37b487`.

## Actual local results

The environment was the Linux workspace with Node `v24.22.3`, pnpm `10.32.1`, React `19.3.1`, TypeScript `5.9.3` and esbuild `0.25.12`. Heavy commands ran serially with `NODE_OPTIONS=--max-old-space-size=1024` while holding the coordinated local build window.

| Lane | Actual result | Raw evidence in the archive |
| --- | --- | --- |
| Original public producer | 42/42; exact plan/result membership and order | `raw/producer-01/receipt.json`, `public-build.json`, original stdout/stderr |
| Existing browser observers | Both syntax checks exit 0 | `raw/app-01/*-observer-syntax.*.log` |
| Normal Shenko Prototype generation | Exit 0; both original Prototypes generated | `raw/app-01/app-prototypes.*.log` |
| Ordinary strict App TypeScript | Exit 0 | `raw/app-01/app-strict-types.*.log` |
| Existing Node suite | 59/59; zero failed, cancelled, skipped or todo | `raw/app-01/app-node-tests.*.log` |
| Normal App producer | Exit 0; 12 hashed production assets | `raw/app-01/app-build.*.log` |
| Source and artifact readback | 57 App inputs and six additional Prototype/observer/HTML inputs stable; every actual static file matches the manifest's complete membership, size and hash | `raw/app-01/receipt.json` |

The actual build receipt SHA-256 is `760dd34c1a210e29a85623bde55964893bbe5388ace8b3b41bef82e2e95fedfd`. It is suitable for the unchanged Inspector and Tree observers' normal-build preconditions. Its `sourceAfter` is the same relative-path hash map those observers compute; additional authored Prototype, observer and HTML inputs are recorded separately instead of being omitted from the source binding.

The generated App JavaScript, build manifest and copied upstream Shadcn notices are committed as ordinary producer outputs. The notice changes come from the exact upstream source. No output was edited by hand, no check was skipped and no new tests were added for this pin update.

The previously completed dependency preparation is also retained: an offline install failed because the scheduler tarball was absent, and the subsequent frozen online install completed. These are separate original logs. This local build reused that prepared installation; it is not presented as an independent clean-install or full independent CI rebuild result.

The documentation integrity checker initially reported 1,976 unavailable-parser diagnostics. After installing the exact `scripts/requirements-ci.txt` into an isolated Python environment, its complete report contains **zero errors** and explicitly says `product_tested: false`. The final subprocess return code was not retained across the task-session interruption, so only the read-back report is claimed here. The original report, dependency-install log and final report remain beside the main archive with their own hashes.

## Reproduction and browser handoff

From the exact Proto candidate, run the unchanged producer:

```sh
corepack pnpm@10.32.1 styles:variant-order:generate
node scripts/build/public-packages.mjs --package @proto.ui/adapter-react --package @proto.ui/prototypes-shadcn --package @proto.ui/cli --plan --json
node scripts/build/public-packages.mjs --package @proto.ui/adapter-react --package @proto.ui/prototypes-shadcn --package @proto.ui/cli --json
```

With `PROTO_UI_SOURCE` pointing to that checkout and `PROTO_UI_BUILD_RECEIPT` pointing to the actual successful producer JSON, the existing `crates/shenko-ui/web` path is:

```sh
node build.mjs --prototypes-only
node node_modules/typescript/bin/tsc --noEmit
node --test --test-concurrency=1 tests/*.test.mjs
node build.mjs
```

The exact executed arguments, environment facts, source diff, timestamps, exit codes and log hashes are in the two receipts and reproduction drivers. The drivers preserve failed outcomes and use fresh output directories; they do not synthesize a successful receipt from a manifest.

The new-pin Inspector, Tree and original reading browser journeys are **not executed by this local receipt**. The unchanged Inspector and Tree scripts keep `chromiumSandbox: true`, the existing input and timing conditions, and the source/asset guards. No browser fallback flags or security changes were introduced. A browser executor can use the retained `raw/app-01/receipt.json` as `PUI_INSPECTOR_BUILD_RECEIPT` or `PUI_TREE_BUILD_RECEIPT`, together with the exact archived source and normal generated assets, and record the actual new outcome separately.

The prior [Resizable](../2026-11-23-inspector-resizable/README.md) and [two-Tree](../2026-11-23-inspector-tree/README.md) browser receipts retain their original source pins. They are not relabeled as new-candidate successes. Physical hardware, AT, a live backend/model, Rust binary rebuild/start, the independent fresh-output CI comparison, the complete Resizable/Tree contracts and the full Harness matrix retain their own acceptance conditions. The upstream Tree contract's Draft status and the App's controlled-empty selection policy are unchanged.

## Preserved archive

[validation.json](validation.json) records the measured local lanes and the separate browser status. [manifest.json](manifest.json) lists 141 payloads with lengths and SHA-256 values. [consumer-evidence.tar.gz](consumer-evidence.tar.gz) contains the original logs and receipts, exact drivers, source snapshot, generated Prototype artifacts and private build-time compiler bundle, and the actual normal production assets.

The archive is **1,571,164 bytes**, SHA-256 `0269be4b7bb1e02c5f86447b230cb2bc8c2bf9b6fd6ad4b7716acc582e7bf343`. Every compressed member was decoded and checked against its original hash after creation. The source remains in the existing Git history with the original authors and commits intact.
