# Node-WASIX: relocate the fixed upstream header link

The [second actual source build](../node-wasix-source-hosted-43945415290/README.md), run `43945415290`, remains **failed**. Its original ZIP and logs show the compiler already passed `-I../deps/v8/include`, but `builtins-typed-array.cc:17` could not open `wasi/wasi-simdutf-compat.h`. No third source build was started for this change.

## Exact source repair

The fixed source is `Multi-V-VM/node-wasix32@20871d1fb49e1af91b1fc729b0cf4e119c0bfd6f`, tree `1f91f029db924986b371c4b848fb65ffd080613d`. Its `deps/v8/include/wasi/wasi-simdutf-compat.h` is a mode `120000` symlink whose original 81 bytes point to `/home/victoryang00/node-wasix32/deps/v8/third_party/simdutf/simdutf-wasi-compat.h`.

The [second named patch](../../../crates/shenko-wasm/ports/node-wasix/patches/0002-relative-simdutf-header.patch) changes only that link's target to `../../third_party/simdutf/simdutf-wasi-compat.h`. The original same-tree header remains unchanged. It neither adds an include search path nor substitutes a header implementation.

| Identity | Exact value |
| --- | --- |
| Original symlink blob | `f7e59ed84434ac2100e830dfc652203f1ccdad48` |
| Relative symlink blob | `3589e61e26b635c6f1620181732b9d04dea83fa8` |
| Original same-tree header blob | `e855e0ab27eecb80fabb1c05d1abb08caaf3c4d8` |
| Header bytes / SHA256 | 1449 / `6412d368a2d00ea303e79331ce998b36ebafa1c2f1d3694fb03ff33abd5ac8f9` |
| New patch SHA256 | `105193b2dc85af5297e3fbd266b42441dc5b4baef26be0dff639cb7d1aa8ecdc` |
| Actual probe builder SHA256 | `48836fc0958df4a8297397a834f16241d4f4181f588313132f49f84640508614` |

The earlier archive repair remains the same independently named `0001` patch. The builder checks both exact preimages and postimages, verifies the header target is a bounded regular file within its owned checkout, records the original symlink bytes without following the broken absolute link, and verifies that the repaired symlink resolves to the exact unchanged header. The combined tracked diff must contain only the original GYP change and this symlink change. A successful build must later match that same combined diff before producing its build record.

Manifest and build record retain both patch identities and their upstream commit/tree. The actual failure header source and its real target now join the retained critical source files. The old archive patch/diff originals keep their old names; the combined diff and link record have separate names. The actual resources, fixed toolchain assets, deadline, original runner/workflow, driver, frames, three cells and reopen checks are unchanged.

The preceding [complete source audit](../node-wasix-source-hosted-43945415290/observation.json) checked all **8** fixed-tree mode `120000` blobs. Only this one has an absolute target. The c-ares relative `config/ares_config.h` target is absent at its static tree location; this repair does not infer its generated build state or change it. The other relative links are preserved, including the deliberate test fixtures.

## Three finite, actually executed checks

The [bounded verifier](../../../crates/shenko-wasm/ports/node-wasix/verify_relative_header.py) builds three minimal synthetic Git repositories from independently verified original bytes. These repositories are explicit fixtures, not a claim to have checked out or compiled all of Node. It calls the actual two builder patch helpers, with ordinary Git `apply --check` / `apply --whitespace=error-all`.

| Risk window | Actual result |
| --- | --- |
| Original absolute link in a relocated checkout | Both patches apply; symlink mode and relative target are exact; opening the link reads the original 1449-byte header; the full tracked diff has only the two recorded paths |
| Original link target has changed | Exact preimage refusal before the link patch runs; existing link and header bytes remain unchanged |
| Same-tree header bytes have changed | Exact target refusal before the link patch runs; existing link and changed header bytes remain unchanged |

The command, source bindings and limits are in [validation.json](validation.json); [probe summary](probe/summary.json), [all subprocess commands](probe/commands.json), [complete stdout](probe.stdout) and [stderr](probe.stderr) preserve the executed results. The exact builder and verifier used are retained in `probe/`. The probe exited 0, with all three expected outcomes. AST comparison independently confirms that the resource budget, `BUILD_SECONDS`, assets, downloader, archive rule check and first patch helper are unchanged; the entire runtime runner and workflow also match the probe's base `10d4a325560a1054fbde00162e8e7696007c2eab`.

No Node/WASIX compiler, module or guest was run for these checks. No capability is installed or advertised by this patch. Subsequent compiler/linker defects may still exist, and Node/Bun installation, runtime recovery and named-target acceptance remain unproved. The documentation parser and Rust toolchain are absent in the recovered environment; the source/evidence checks above do not replace those gates. [archive-index.json](archive-index.json) records all retained bytes.
