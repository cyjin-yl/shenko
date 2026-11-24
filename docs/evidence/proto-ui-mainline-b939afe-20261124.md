# Proto-UI mainline consumer upgrade — 2026-11-24

## Exact source and scope

Shenko advances the reviewed Proto-UI pin from
`f24720f870cd76f86c81ee8343359486afd3f0de` to
`b939afe35d30a1a436766d80beea7093a46e4fb2`, the upstream main commit
that merged [Proto-UI #1782](https://github.com/Proto-UI/Proto-UI/pull/1782).
Application base: `6f0b266d9788c18bcd514487bb2b2646a7064262`.
The pin, actual compiler inputs, generated browser resources and manifest
refer to the same source revision; `migrated_from` names the actual old pin.

The adapter, Shadcn and CLI packages remain `0.3.0-alpha.2`; the upstream
lock digest remains
`6e21c4b8444ca5e4a2e427287a76a346137b65d76403579942269bfea1fc7fe8`.
The private compiler has no registry SemVer. This change updates its exact
source revision, retaining the audited build-time source seam,
`react-dom-source-v1`, IR 8 and `runtime_imported: false`.
Upstream's independent source-pinned artifact route is not a registry release.

## Observed checks

| Check | Actual result |
| --- | --- |
| Frozen upstream and consumer installation | Passed; lockfiles unchanged |
| Public dependency closure | 42 packages built |
| Authored Prototype regeneration | transport-badge and message-bubble, IR 8 |
| Consumer TypeScript | `pnpm run check` passed |
| Existing Node tests | 60 passed; 0 failed, skipped or cancelled |
| Normal production build | Passed; 12 hashed static resources |
| Inspector browser observer | Four existing cases passed |
| Tasks / Playground Tree browser observer | Four existing cases passed |
| Browser errors | No page or cleanup errors in either observer |

The build used an isolated development container, shared heavy-work admission,
a 4 GiB memory limit and two CPUs. Toolchain: Node 24.22.1, pnpm 10.32.1,
TypeScript 5.9.3, esbuild 0.25.12, React/React DOM 19.3.1.
The admitted consumer source snapshot was unchanged by the build.

The unchanged observers ran sequentially as non-root `ezra` on doesvm with
Playwright 1.58.2 and Chromium 145.0.7632.6, keeping
`chromiumSandbox: true`. Both checked this build's source, pin, lock,
Prototype provenance and resource manifest before exercising the App.
Inspector covers pointer/keyboard resizing, bounds, collapse/restore,
responsive cancellation and focus. Tree covers desktop tasks, desktop
artifacts, phone invalid-data/recovery and subject retirement.

The full host `/tmp` prevented the initial heavy-work admission from
starting a build. The successful attempts used a task-owned temporary
directory on the persistent project volume; no unrelated files were removed.
A local documentation-check attempt lacked the parsers declared by
`scripts/requirements-ci.txt`; it is not counted as a passing check.

## Evidence and acceptance boundary

[Raw evidence archive](proto-ui-mainline-b939afe-20261124.tar.gz)
contains the executed build driver, stage commands/exit codes, public build
receipt, normal-build source binding, type/test/build logs, generated
manifest and original browser reports/screenshots.

Archive SHA-256: `d46512cd8b9e17693f76008584c5279ffb9d6e390b49a5310637e174f84d6692`.

The browser observers use the actual App with its existing demo or
deterministic HTTP/WebSocket read fixtures. They do not claim live
backend/model, physical touch hardware, assistive technology, or complete
portable-family acceptance. The manifest still records two compiled authored
Prototypes and 12 upstream adapter families; subsequent migration remains
tracked by [#335](https://github.com/cyjin-yl/shenko/issues/335).

The interface contract now binds the old 0/216 inventory and 40 KiB measurement
to their original 2026-11-13 revisions. No new inventory percentage is inferred.

CI independently rebuilds the committed resources from fresh inputs through
`scripts/ci/web_assets.py prepare/check`. Its actual result is recorded on
the delivery PR, separately from the local checks above. All six required
repository checks remain mandatory.

## Reproduce

Use `.node-version` and both lockfiles. The archived `build/build.py`
contains the executed commands: frozen installs; the upstream
`scripts/build/public-packages.mjs` closure for adapter-react,
prototypes-shadcn and cli; then consumer `pnpm run check`,
`pnpm run test` and `pnpm run build`.
Set `PROTO_UI_SOURCE` to the exact upstream checkout and
`PROTO_UI_BUILD_RECEIPT` to its real public-build JSON.

The source-bound `normal-build.json` is the matching
`PUI_INSPECTOR_BUILD_RECEIPT` / `PUI_TREE_BUILD_RECEIPT` for the existing
`web/scripts/inspector-browser.mjs` and `tree-browser.mjs`.
Provide a sandbox-capable `PUI_CHROME_EXECUTABLE`,
`PUI_PLAYWRIGHT_MODULE`, a fresh `PUI_EVIDENCE_DIR` and a writable
`TMPDIR`. Keep the original assertions and launch options.
