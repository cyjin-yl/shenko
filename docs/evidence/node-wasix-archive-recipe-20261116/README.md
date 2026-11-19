# Node-WASIX archive command boundary — 2026-11-16

The original source-build [run 43922989826](https://github.com/cyjin-yl/shenko/actions/runs/43922989826) remains failed. Its complete artifact and original observations are in [the source-build archive](../node-wasix-source-hosted-43922989826/README.md). No new Node build or guest run was started for this change.

## Observed cause and scoped repair

The original generated Makefile defines `exact_echo` as one `printf` argument containing an expanded command. Its `do_cmd` path prints and caches that command. The complete original compile stdout records 928 distinct OpenSSL object output paths; joining just those paths needs 138,346 bytes. The actual CI error is `make: printf: Argument list too long` at the OpenSSL archive target. That run did not retain its generated OpenSSL target file or rejected exec argv, so their precise bytes remain unavailable.

The independently fetched, exact [upstream GYP generator](https://github.com/Multi-V-VM/node-wasix32/blob/20871d1fb49e1af91b1fc729b0cf4e119c0bfd6f/tools/gyp/pylib/gyp/generator/make.py) already supplies the Linux archive helpers for WASI/wasm. However, only Linux/Android select their response-file recipes for static libraries; WASI/wasm enter the long `do_cmd` archive route. The two-line patch selects the same existing response-file helpers for WASI/wasm. Normal `crs` and thin `crsT`, the filtered object order and compiler/source/guest inputs are retained.

This recipe is used only by a fresh output-directory source build. The existing Linux response-file helpers use ordinary dependency-based Make rules and do not retain `do_cmd`'s command-change dependency tracking. Incremental rebuild behavior is not claimed or accepted by this diagnostic.

## Actual local observations

The finite [reproducer](../../../crates/shenko-wasm/ports/node-wasix/verify_archive_recipe.py) uses the original generated Makefile definitions, the 928 observed object names relocated under its own directory, and hard links to one real newly compiled native C object. The local joined names are 142,986 bytes. These are explicit native fixture objects, not the original OpenSSL objects or a WASIX toolchain.

| Window | Actual result |
| --- | --- |
| Original `do_cmd,alink` recipe | make exits 2 with `printf: Argument list too long`; no archive is produced |
| Existing normal response-file recipe | native GNU ar exits 0; response bytes equal all 928 input paths in order; archive membership matches all 928 basenames in order |
| Existing thin response-file recipe | native GNU ar exits 0; the same 928 response inputs and ordered archive members are preserved |

[Probe 2 summary](probe-2/summary.json) records the actual 19:31:36–19:31:41 UTC run and output hashes. Complete commands, stdout/stderr, generated fixture Makefiles, response files, original observed path list, native object and exact executed probe source are retained. This demonstrates the local command-argument boundary and selected archive mechanism. It does not prove wasixar/LLVM compatibility, a successful final Node link, any of the three original cells, restore, Shenko installation or Bun.

The initial probe requested syscall tracing. The environment denied `PTRACE_TRACEME`, so it exited before the make target ran. [Probe 1](probe-1/summary.json) preserves that failure, the empty trace and its exact original script. The subsequent probe uses ordinary process execution and explicitly reports `syscall_trace_captured=false`; no tracing permission was bypassed and no original CI argv is claimed recovered. After the successful probe, source review made the diagnostic's interrupted-process reap finite as well; that error-path change received syntax review, not an additional exercised timeout claim.

## Source identity and next build

The fixed patch, its preimage and postimage hashes are embedded in the builder. A local single-file synthetic Git checkout exercised the actual patch-application helper and matched the exact upstream/postimage bytes; [source-check/checks.json](source-check/checks.json) distinguishes that fixture from the upstream repository. Python syntax checks passed and the resource-budget AST and runtime runner bytes remained unchanged.

A future source build must verify the upstream commit/tree, patch identity and exact pre/postimage; retain original and patched GYP, the applied diff and actual OpenSSL target/response file; check that configure selected the response recipe; and confirm that the tracked-source diff after compilation equals the recorded patched input. The build record now states that it matches the recorded patch instead of falsely claiming a pristine upstream tree. Compiler assets, deadlines, resource guards, original driver/framing/three cells and restore predicates are unchanged.

The next patched source build is still unexecuted. The sole completed build's failure remains attached to its original candidate and cannot be changed into success by this local native-archive result.
