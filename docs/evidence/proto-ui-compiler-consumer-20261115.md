# Proto-UI compiler and Shenko consumer — 2026-11-15

This increment advances the exact
[consumer pin](../../crates/shenko-ui/web/upstream-pin.json) from
`41282cb0ddc39283826e52e9932be123fc83eb47` to
`7521cd501f454b4e013ec6e9656fb4b6e4951514`, the published head of
[Proto-UI PR #1461](https://github.com/Proto-UI/Proto-UI/pull/1461).
At the pre-build live check that PR was open and ready, and upstream main was
`ff24ed1a9e6cfa3486a2fdcc86b890556265e339`. The selected source is that mainline
plus the two narrow compiler commits below; it is not described as a merged
release. `migrated_from` records the actually replaced `41282cb0` pin.

The upstream lock remains
`3ac437fda5666f744c02587d9c90b47a927a6e2521973f6a18b47cc3c7cbe422`.
The three public entry versions remain `0.3.0-alpha.1`. Existing `build.mjs` and
`compiler.mjs` continue to verify the exact checkout, lock, package identities,
private source compiler and generated Prototype provenance before bundling.
The private compiler is still a build-time source dependency; it is not added
to the public package selection or imported by the browser.

## Why this source

The earlier pin's full compiler job had three failures: the strict generated
Vue2 graph and two undefined-child diagnostic expectations. The selected
candidate preserves the actual source and semantic contracts:

- `d8e965b8a044f71761febe093360c9c9ddcd2602` imports only the exact
  `vue2-source.ts` authored in `319d6c3215856238e30e695024d1deeccea6e5a2`.
  It retains the original author, timestamp and existing certification, plus
  the importing contributor's certification. The generated owner binds exact
  state handles to their typed external view and checks an interactive HTML
  root before use. Strict generated-source checks remain unchanged.
- `7521cd501f454b4e013ec6e9656fb4b6e4951514` corrects the two existing
  undefined-child refusal assertions against actual parser history and the
  template boundary. Mainline exception-flow support already recognizes
  `undefined` as a void value. A void child remains rejected; the tests now bind
  its precise PUI1006 category, message and original source span instead of
  expecting an unresolved identifier. No input is newly admitted or skipped.

Compared with the previous pin, the intervening merged mainline also contains
React Text Control name/default-value and lease handling, the public CLI's
style-helper traversal optimization, and shared Select-row sizing. The actual
42-package rebuild consumes those mainline changes; the whole pin transition
is not represented as only a two-file delta from the older pin.

The broader draft #1460 was not adopted. An earlier full import changed an
unrelated historical Context helper and failed exact-corpus verification; its
expected hash was not replaced. The selected Vue2 file is byte-identical to its
authored source, while that draft's unrelated Context, project-output and
component changes remain outside this pin increment.

## Compiler evidence and preserved failures

| Candidate and environment | Actual result |
| --- | --- |
| Previous Shenko pin `41282cb0`, final [Nightly run 43717822993](https://github.com/cyjin-yl/shenko/actions/runs/43717822993) | Rust long job passed; compiler reported 1511 passed, 3 failed, 1 skipped. This failed compiler result remains historical evidence. |
| Earlier [Nightly run 43717240694](https://github.com/cyjin-yl/shenko/actions/runs/43717240694) | Compiler reported 1510 passed, 4 failed, 1 skipped, including an additional historical-migration timeout. The later run's passing migration check does not erase this earlier timeout. |
| Rejected broad local import `87dcb10448431200168af5bc882c1ccd0c7e1022` | 1377 passed, 2 failed, 142 skipped, plus an unexpected worker exit. The changed Context fixture, stale assertions and local Chrome socket/ptrace EPERM are retained as distinct causes. |
| Narrow source `7b9de156f5878bac5d734c123386075767142c12`, local Node 24.22.1 | The three focused existing files passed 112/112, including the actual strict Vue2 graph. |
| Same narrow source, isolated Box checkout; Node 24.21.0, pnpm 10.32.1, Chrome 157.0.8103.38 | Full compiler suite: 76/76 files, 1514 passed, 0 failed, 1 existing skip; exit 0 in 235.78 seconds. Real browser suites ran with the existing harness flags. |

The final `7521cd5` candidate changes only commit certification metadata from
the tested `7b9de156f` candidate. Both complete Git trees are exactly
`2c62b5d265622e847d3995f914fa702cde3f2533`. The full Box result remains bound to
the SHA that actually ran; tree equality is recorded separately. The single
existing skip is old-corpus generation in `capture-old` mode, which is inactive
during normal verification. No new compiler test or skip was introduced.

The retained full Box log is 481893 bytes, SHA256
`34e14d8a6f0e001b2af27008401468ffc58afe8ece850c05e3b24c75f2d86c55`.
An exact `7521cd5` public closure also built all 42 packages on Box, but its
Node 24.21.0 result is separate from the pinned local consumer build below.
A passing ordinary upstream CI whose heavy jobs were skipped is not counted
as a full hosted compiler run.

## Integrated Shenko generation

The integrated source generation completed at `2026-11-15T16:44:37.297Z` with
actual Node `v24.22.1`, pnpm `10.32.1`, esbuild `0.25.12`, TypeScript `5.9.3`
and React/React DOM `19.3.1`. It consumed the frontend at integrated Shenko
head `25207eb7e2b8bd12adc1d538fa0c54e989e9aeca` plus the new pin. The frontend
input inventory remained unchanged throughout the build; unrelated Rust
integration does not form part of these frontend inputs.

| Check | Actual result |
| --- | --- |
| Frozen dependency installation | Both upstream and consumer installed with `--frozen-lockfile`; the reviewed upstream lock remained byte-identical. |
| Public build plan and actual closure | The existing `scripts/build/public-packages.mjs` built all 42 planned packages, in the planned order, containing the three public entries and excluding the private compiler. |
| `pnpm run check` | Both Shenko Prototypes were compiled from current authored bytes; `tsc --noEmit` passed. |
| `pnpm run test` | 34 passed, 0 failed, 0 skipped. No new test scenario or changed assertion was added. |
| `pnpm run build` | Existing `build.mjs` and `compiler.mjs` produced the app, styles, bootstrap, licenses and manifest successfully. |
| Existing `validate_static` and `compiler_files` checks | All 10 static payloads matched membership, byte counts and SHA256; 18 compiler/Prototype source and provenance facts matched their actual files. |

The source installation replaced only the consumer's ignored `@proto.ui`
symlinks whose targets were verified to belong to the previous owned checkout.
The unchanged builder then linked the selected checkout. No package source,
build script, expected hash or toolchain field was hand-edited to pass.

The [generated manifest](../../crates/shenko-ui/src/static/build-manifest.json)
retains the complete asset inventory and exact compiler/Prototype closure.
Only `app.js` changed among the ten payloads; styles, theme, bootstrap and
license payloads remain byte-identical to the previous generation.

| Retained identity | SHA256 |
| --- | --- |
| Actual local 42-package build receipt | `a4aac6048daf4369e5c0054676de344bb77d0db8bcdfbc698886d25a76712c36` |
| Generated manifest | `a8a3f9bd4d896828b2ec42510ba6e558a60d1fadf60d6fd8f046ced6b41a6762` |
| Browser `app.js`, 1132502 bytes | `a2a71f5fdf1baca8239cb44dfd6aea9f8a7b2bf29f387088ea97c1e65ab74f10` |
| Reviewed compiler source tree | `3317af48496bc0ed0c6d447c11223f23b60a769732f39e4f112c36cc127fa18c` |
| Build-time compiler bundle | `c915bf31223a1f951b99dd23d69907638c81291583a195ddd141e0201dfb15e6` |

The local logs, actual package receipt, input inventory and verification record
are retained under
`/workspace/scratch/4de31219f954/evidence/continuation/web-certified/`.
This generation used the shared heavy-build lock and existing installed source
checkouts; it does not claim to be the independent clean CI reproduction.

## Acceptance boundary

This evidence concerns the selected compiler source and the actual integrated
frontend generation. A fresh independent rebuild and full compiler run on the
final published Shenko head remain separate CI results. The existing fresh
rebuild checker compares payload bytes and compiler/Prototype provenance; no
expected digest, stable toolchain field or assertion is edited to obtain a
match. This increment does not claim a complete UI visual/streaming acceptance,
real model/OXR/channel/seat acceptance, or long-run product reliability.
