# Clippy input-scope audit: the classification remains unresolved

This is a read-only audit of the seven source-policy failures from
[CI 43968540714](../ci-3e62c0e5-20261116/README.md). It changes neither the
gate nor its inputs. The original public rusqlite files, their paths and
attributes, the independent baseline, ignored-test policy, warning counts
and compiler flags remain unchanged. **No complete compiler-input manifest
was captured, and no exemption or passing result is inferred.**

The source inspected here is `b10ca4c4064d7cc5d877250f8de834dc4d0dbea5`.
The relevant gate, workflow, UI build script, UI include site and cross-package
test include have the same Git blobs as public candidate
`3e62c0e517a246872101cd1e61a694b2156bc919`; see
[the exact local observation](local-availability.json). The original CI's
actual checkout is `1c01c95a4915856312746b5c7ef404c1d7a84cec`, with baseline
`84ae19404f2a17d259362c99091db408b7262e14`. This audit ran no Cargo, compiler,
product test, model or remote workflow and adds zero behavioral scenarios.

## What the existing gate actually consumes

[`source_policy`](../../../scripts/ci/clippy_ratchet.py) calls `git ls-files`
and lexically examines every tracked `.rs` file before either compiler
comparison begins. It also checks Cargo lint tables and configuration bytes.
It does not consume Cargo metadata, compiler artifacts, dep-info or file-read
observations when selecting Rust files. Thus adding a verbatim upstream source
as evidence produces new path/attribute pairs even without evidence that the
source is compiled as part of a workspace target.

The two original files still exactly match their earlier
[frozen inventory](../ci-83bf3914-20261116/capture-readonly-review/inventory-final.json):

| File under the original archive's `sources/` | Bytes | SHA-256 |
| --- | ---: | --- |
| `rusqlite-0.32.1-inner_connection.rs` | 15,025 | `3c49805fe5a239c89ff512f603c6f3f514061d653e3fcb5e7b46af1887a98382` |
| `rusqlite-0.32.1-row.rs` | 19,977 | `0a246577a097e4304a08baf54bac3c0555e3aa7ea53589d33218e384a76d6789` |

The seven original source-policy errors are retained verbatim in
[original-analysis.json](original-analysis.json). Being a preserved upstream
original proves provenance; it does not itself prove that a future `#[path]`,
`include!` or compile-time program cannot consume that path. No provenance-based
exception is introduced.

## What the actual compiler streams prove

[inspect_original.py](inspect_original.py) reads the unchanged original Clippy
ZIP directly, checks its exact 2,735,684-byte size and SHA-256
`f6de891ffe2529c1d8de780939ed221bd15d5c5a24dbac33c11f38a04d75f986`, and reads
all 18 members through ZIP CRC validation. It retains per-member SHA-256 and
derives the following counts from the complete JSONL records. It does not
extract or execute an archive member.

| Stream | Records | Artifacts | `fresh: true` | `fresh: false` | Build-script messages | Build finished |
| --- | ---: | ---: | ---: | ---: | ---: | --- |
| Baseline production | 1,457 | 626 | 587 | 39 | 77 | true |
| Candidate production | 1,456 | 626 | 587 | 39 | 77 | true |
| Baseline all targets | 9,970 | 752 | 587 | 165 | 77 | true |
| Candidate all targets | 10,020 | 754 | 587 | 167 | 77 | true |

Each stream represents 24 distinct proc-macro artifact packages. These are
artifact observations, not a claim that every macro body executed. The retained
artifact fields give a target's root `src_path`, features, profile and output
filenames. They do not enumerate all source files read. The original ZIP has
no dep-info or process file-access trace.

Cargo's [JSON interface contract](https://doc.rust-lang.org/cargo/reference/external-tools.html#artifact-messages)
states that `fresh: true` means rustc was not run. It also states that a
`build-script-executed` message may contain cached output without a new script
execution. A successful build and a replayed message therefore do not establish
fresh file observations. The existing workflow restores `.ci/target/clippy`;
there is no matching retained input manifest for its reused artifacts.

## Why the obvious narrower scans are insufficient

| Proposed evidence | Actual boundary |
| --- | --- |
| Workspace package directories | The existing native E1 integration test uses `#[path]` to import a Store fixture across package boundaries. Directory membership is not a Rust inclusion boundary. |
| `compiler-artifact.target.src_path` | This is the crate root, not its module/include closure. Warning-free sources need not appear in diagnostic spans either. |
| Only tracked files or only `.rs` files | The UI's real `build.rs` writes `OUT_DIR/browser_modules.rs`, which `routes.rs` includes. Generated source is not necessarily tracked; `include!` need not use a `.rs` suffix. |
| Rustc/Cargo `.d` files alone | These describe tracked build dependencies. Arbitrary procedural-macro or build-script reads are not made complete by parsing that list. Build scripts explicitly report change dependencies; arbitrary macro code has the compiler's file access. |
| Trace the commands that happen to run after a cache restore | Cached artifacts and generated outputs may embed earlier inputs that are never opened in this run. The current cache carries no separately verified provenance closure. |
| One set of target/feature observations | The existing commands select native production and all-target builds with `shenko/compat-poppy`. This does not prove unused code under other features, target cfgs or inactive branches is unreachable. A manifest must bind its actual configuration and cannot silently exclude unobserved sources from a broader policy. |

The relevant concrete call sites are
[`bin/shenko/tests/native_evidence_delivery.rs`](../../../bin/shenko/tests/native_evidence_delivery.rs),
[`crates/shenko-ui/build.rs`](../../../crates/shenko-ui/build.rs), and
[`crates/shenko-ui/src/routes.rs`](../../../crates/shenko-ui/src/routes.rs).
These are existing consumers, not new adversarial test fixtures.

The primary references are Cargo's
[dep-info description](https://doc.rust-lang.org/cargo/reference/build-cache.html#dep-info-files),
[build-script change-detection contract](https://doc.rust-lang.org/cargo/reference/build-scripts.html#change-detection),
and the Rust Reference's
[procedural-macro execution model](https://doc.rust-lang.org/reference/procedural-macros.html).
The latter permits normal compiler-level file access during macro execution.
The currently published [`proc_macro::tracked::path` documentation](https://doc.rust-lang.org/proc_macro/tracked/fn.path.html)
describes explicit tracking and is labelled nightly-only in the documented
1.100.0 toolchain; it is not evidence of automatic tracking by CI's pinned
1.98.1 compiler. No unstable compiler flag or bootstrap escape was enabled.

## Observation capability is missing here

The inspected tree has no tracked devcontainer or Dockerfile definition.
The Clippy workflow declares the `rust:1.98.1-bookworm` job container, installs
Python, CMake, Clang, pkg-config and OpenSSL development files, and installs
the pinned Clippy component. It does not install or invoke a complete process
file-access observer or declare tracing capabilities. Its restored build
cache has no input-provenance consumer.

Locally, Cargo, rustc and rustfmt are absent. `strace` exists, but an earlier
bounded diagnostic on the same reported kernel failed before observing the
target command: `PTRACE_TRACEME` and `PTRACE_SETOPTIONS` returned
`Operation not permitted`. Its unchanged [stderr](prior-ptrace-legacy.stderr),
[original command record](prior-ptrace-commands.json) and
[original result](prior-ptrace-summary.json) are retained here. That original
probe concerned native archive command lengths, not Cargo. It is reused as
an observed tracing limitation, not relabelled as a compiler experiment.

The new read-only availability observation reports UID 1000, empty effective
and bounding capability sets, `NoNewPrivs: 1`, `Seccomp: 2`, and no local
Docker, Podman, auditctl or bpftrace. Those fields do not by themselves prove
which specific seccomp rule rejected the earlier trace. No new privilege
request, tracing probe, toolchain installation or workflow dispatch was made.

## The smallest defensible next implementation boundary

A future input-scope change needs an actual observation producer and a gate
consumer together. A manually declared source list, a dependency-list parser,
or a candidate-supplied `complete: true` field is insufficient. The following
is an implementation boundary, **not an implemented or validated mechanism**:

1. Run each existing normal compiler command unchanged under an observer that
   accounts for the entire process tree, including build scripts, compiler
   children and procedural macros. Bind process/exec/cwd identity and successful
   file access to resolved file identity, not only a textual `open()` path.
   Inherited descriptors, symlink/hardlink aliases, memory mappings, generated
   files, deleted files and unhandled I/O mechanisms must not silently disappear.
2. Bind observations to immutable source bytes, the exact compiler/toolchain,
   Cargo command and lint flags, environment/configuration, target/features
   and generated-output identity. Observe generated content before it can be
   replaced; hashing only the final `OUT_DIR` is not a proof of earlier reads.
   Missing events, truncation, unknown paths/aliases, unsupported access or
   interrupted observers make the manifest incomplete.
3. Reuse a compiled artifact only with its original verified input manifest
   and matching identity closure. Otherwise build in a new empty target area
   under that same observer, leaving prior cache bytes and receipts intact.
   Freshly compiling just workspace roots does not establish the provenance
   of old build-script executables, proc-macro libraries or generated outputs.
4. Make `source_policy` consume only a validated, configuration-bound result.
   Keep new active suppression/ignore checks and the existing compiler count
   comparisons intact. Unknown or unobserved scope remains a failure; no
   filename, documentation directory, extension or upstream-source exemption
   may make it pass. Unbuilt configurations remain explicitly unproved, and
   the existing broader tracked-source protection must not be narrowed by
   silently treating them as unreachable.

A generic `strace -f -e trace=file` wrapper alone does not establish all of
those properties. It supplies neither a prior-cache closure nor necessarily
the bytes read through inherited descriptors or later-mutated generated
files. The current environment cannot validate the missing full process
observer, and the existing retained run cannot be retroactively instrumented.
For that reason this checkpoint deliberately leaves the seven failures in
place instead of shipping a directory filter or an unconsumed manifest API.

## Checks performed

The local Python analysis completed with exit 0 on the original ZIP. Its
original-member digests, stream field inventory and counts are in
[original-analysis.json](original-analysis.json). The two public rusqlite
files match their frozen inventory exactly. Source blob comparisons and
available tools/capabilities are in [local-availability.json](local-availability.json).
The complete original ZIP and compiler streams remain in the existing
[CI archive](../ci-3e62c0e5-20261116/README.md), without duplicate binaries here.

The audit script is only an evidence reader. It does not approve input scope,
run the gate, or alter any original compiler result. Local Python syntax and
whitespace checks are not Rust compilation, source-policy verification, or
product acceptance.
