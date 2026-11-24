# Sprint 2: main integration and actual f247 Inspector/Tree consumer

This continues draft Shenko #402. The working candidate was the real merge of
c240169239bfb55a8ee47d3129c3fd820e1de747 and current main
ab0d3dfb3474a9bd9e0f459b46c48d53857cdb4b. The only conflicting generated build
manifest was resolved by the ordinary producer using the exact Proto-UI pin
f24720f870cd76f86c81ee8343359486afd3f0de. The rebuilt JavaScript, CSS and manifest
are committed as produced. The source and ownership of the existing Resizable
Inspector and Tasks/Playground Tree domains are preserved.

## Actual consumer results

| Lane | Result |
| --- | --- |
| Three declared public dependency seeds | 42/42 packages built; exact plan/result order |
| Shenko authored Prototype generation | Both transport-badge and message-bubble built |
| App strict TypeScript | Exit 0 |
| Existing Node tests | 59/59; no skip, cancellation or todo |
| Ordinary App production build | Exit 0; all 12 manifest assets verified |
| Resizable Inspector | Four original browser journeys passed |
| Tasks and Playground Trees | Four original journeys; 67/67 checks passed |
| Source and generated asset guards | Stable before and after actual execution |

The three producer seeds are @proto.ui/adapter-react, @proto.ui/prototypes-shadcn
and @proto.ui/cli. They select 42 of the 47 public packages. The first preparation
attempt incorrectly compared the unfiltered 47-package plan with this 42-package
closure and stopped before building. Its original result and log remain in the
archive. The subsequent ordinary selected-plan build is a separate successful
execution; neither its plan nor its receipt was synthesized.

The consumer pin moves from 5cb80eb937f20982dd7dc8cbc93a90e91d665c31 to f247.
Its three declared package versions remain 0.3.0-alpha.2. The lock SHA-256 remains
6e21c4b8444ca5e4a2e427287a76a346137b65d76403579942269bfea1fc7fe8.
The producer had its own frozen installation and remained clean at f247.
Resizable and both Trees still use their public Shadcn exports with the React
adapter. The Compiler is a build-time source-tree dependency for the two authored
Prototypes, not a browser runtime import or a claimed published Compiler package.

The Inspector observer exercises public pointer/keyboard input and pixel bounds,
collapse/restore focus and width preference, responsive contact cancellation, and
replacement-handle rejection of late input. The Tree observer exercises Tasks,
Playground artifacts, phone invalid-data recovery and subject retirement. Both
unchanged observers ran in Chromium 145.0.7632.6 with chromiumSandbox enabled.
The actual console, cleanup, source, HTTP/WebSocket fixture and asset observations
are retained. These fixtures exercise the real App/controller path; they do not
claim a live backend, physical hardware or a screen-reader session.

## Reproduction and source binding

The complete executed argument vectors, environment, timestamps, exit codes,
source hashes, production assets, original observers and screenshots are retained
in [consumer-evidence.tar.xz](consumer-evidence.tar.xz).
[manifest.json](manifest.json) lists every original payload and SHA-256;
[validation.json](validation.json) records the measured lanes and exact receipt
locations. Every archive member was decoded and compared byte for byte after
creation.

Run the ordinary Proto-UI public package producer at the pinned commit with
--package @proto.ui/adapter-react --package @proto.ui/prototypes-shadcn
--package @proto.ui/cli. Point PROTO_UI_SOURCE and PROTO_UI_BUILD_RECEIPT at that
clean checkout and its actual producer JSON. In crates/shenko-ui/web, execute the
existing Prototype build, TypeScript check, Node tests and normal App build.
The unchanged inspector-browser.mjs and tree-browser.mjs accept the retained
normal-build receipt through PUI_INSPECTOR_BUILD_RECEIPT and
PUI_TREE_BUILD_RECEIPT. Their complete launcher environment is in the archive.

Earlier evidence keeps its original source pin and outcome. This new success does
not rewrite the previous preparation failure or older browser receipts. The
complete Resizable adjustment table, portable Tree controlled-state and AT
contract, independent CI asset rebuild, Rust application, live service, original
Harness scope, independent review and actual PR integration remain separately
owned acceptance conditions. Draft status is preserved.

The documentation check is recorded separately as documentation integrity only;
it does not mark product requirements accepted.
