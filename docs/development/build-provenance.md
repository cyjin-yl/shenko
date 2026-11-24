# Build provenance: pinned toolchain, offline install, digest manifest, and CI

This page records how Shenko makes a build's inputs explicit: which toolchain
versions are pinned, which lock files and digests bind dependency resolution,
how a prepared machine can install and rebuild without resolving anything new,
and which CI jobs verify the result. It is a development-method and provenance
note, not a product contract and not a completion claim. `scripts/check_docs.py`
checks documentation integrity only; it neither builds nor runs Shenko. The
`docs` CI job therefore proves that documents and links are intact, not that a
product behavior is accepted.

The underlying obligations are [development contract §40.4](../governance/development-contract.md#spec-40-4)
("dependencies and builds are reproducible; releases record real locked
versions") and [web interface §21.2](../specs/web-interface.md#spec-21-2)
(Proto-UI packages move together to one verified commit and are pinned there).
Issue #296 T1/T4 is the review source for this page. The page describes the
implementation at this baseline; the remaining T4 published-artifact item is
listed under [what is still not pinned](#what-is-still-not-pinned) rather than
claimed as done.

## What is pinned

Pins live in the repository, not only in a workflow or an agent's shell
history. A checked-in file is the authority; CI reads that file, and the
commands below repeat the same values.

| Input | Pinned value | Authority | Checked by |
| --- | --- | --- | --- |
| Rust compiler | `1.98.1`, `profile = "minimal"`, `components = ["clippy"]` | [`rust-toolchain.toml`](../../rust-toolchain.toml) | Rust jobs use `rust:1.98.1-bookworm`; [`scripts/ci/clippy_ratchet.py`](../../scripts/ci/clippy_ratchet.py) reads the channel from this file |
| WASI target | `wasm32-wasip1` | added explicitly by the CI and nightly commands | `rustup target add wasm32-wasip1 --toolchain 1.98.1` |
| Node.js | `24.22.1` | [`.node-version`](../../.node-version) | `node:24.22.1-bookworm` containers, `actions/setup-node` with `node-version-file`, and [`scripts/ci/web_assets.py`](../../scripts/ci/web_assets.py) comparing `node --version` to `v` plus this file |
| pnpm | `pnpm@10.32.1` | [`crates/shenko-ui/web/package.json`](../../crates/shenko-ui/web/package.json) `packageManager` | `pnpm/action-setup` reads that field; `web_assets.py` compares the actual `pnpm --version`, and requires the pinned Proto-UI checkout to declare the same manager |
| Cargo dependency resolution | [`Cargo.lock`](../../Cargo.lock) | lock file | `cargo ... --locked` on check, test, build, and `cargo metadata --offline --locked --no-deps` for the boundary check |
| Shenko web dependency resolution | [`crates/shenko-ui/web/pnpm-lock.yaml`](../../crates/shenko-ui/web/pnpm-lock.yaml) (`lockfileVersion: '9.0'`) | lock file | `pnpm install --frozen-lockfile`, followed by `web_assets.py` checking the tree did not change |
| Documentation-checker Python packages | exact versions in [`scripts/requirements-ci.txt`](../../scripts/requirements-ci.txt) | requirements file | `pip install --requirement scripts/requirements-ci.txt` inside `python:3.13.9-bookworm` |
| Proto-UI source | commit `f29738eb6a804ccfc58510cc5a5bd1692690716e`, lock digest `e374ba03f76079fd191d63c2da1f62861b7e90bbeba64a0b310027fc2c15b7f8` | [`crates/shenko-ui/web/upstream-pin.json`](../../crates/shenko-ui/web/upstream-pin.json), mirrored into [`build-manifest.json`](../../crates/shenko-ui/src/static/build-manifest.json) | `web_assets.py prepare` fetches that full commit, checks the lock hash, and rejects a stale or mismatched pin |
| WASI SDK | `wasi-sdk-34.0-x86_64-linux`, tarball SHA-256 `b761e3a0721dbae9c09a0059e5fdb2bf917d1b4a8a7b430fb3b5aafb0984b2c4` | the CI and nightly workflows print the digest and verify it with `sha256sum --check --strict` before extraction | [`crates/shenko-wasm/ports/ssh-wasi/build.py`](../../crates/shenko-wasm/ports/ssh-wasi/build.py) consumes the extracted SDK |
| Proto-UI compiler facts | source-package and entry digests, compiler source-tree digest, bundle digest and byte count, dependency resolutions, generated Prototype and provenance digests | `build-manifest.json` `compiler` block | `web_assets.py` recomputes each digest from the rebuilt bytes |

The Python checker dependencies are version-pinned but are not
hash-pinned (`--require-hashes` is not used). The container images named above
and the GitHub Actions are also referenced by tag (`rust:1.98.1-bookworm`,
`node:24.22.1-bookworm`, `python:3.13.9-bookworm`, `actions/checkout@v4`), not
by image or action digest. Those are recorded gaps, not implicit byte-identity
claims.

## Offline install

Pinning resolution and vendoring bytes are different things. Shenko commits its
locks and digests but does not commit dependency archives, `node_modules`, the
pinned Proto-UI checkout, the WASI SDK tarball, or model files. A fully
disconnected machine therefore needs a prepared cache; it cannot reconstruct
the closure from the Git tree alone. The preparation step is online and the
replay step is fail-closed.

### Prepare the exact closure once

Run these while the network is available. They populate caches only; they do
not change any lock file.

```sh
# Cargo registry and Git closure for the committed lock.
cargo fetch --locked

# pnpm store entries for both lock files. `fetch` populates the store and does
# not create node_modules in the source tree.
(cd crates/shenko-ui/web && pnpm fetch --frozen-lockfile)

# The pinned Proto-UI checkout is obtained by the verifier itself:
python3 scripts/ci/web_assets.py prepare
(cd "$PROTO_UI_SOURCE" && pnpm fetch --frozen-lockfile)

# Documentation-checker wheels.
python3 -m pip download --requirement scripts/requirements-ci.txt --dest <wheelhouse>

# The WASI SDK archive and, for the opt-in real-model workflow, the model and
# llama.cpp archives, together with the SHA-256 values recorded above.
```

`web_assets.py prepare` creates the fresh source trees and records
`inputs.json`; do not install `node_modules` into the fresh trees it creates,
because the later `check` step rejects that reuse.

### Replay without resolving new versions

```sh
cargo check --workspace --all-targets --features shenko/compat-poppy --locked --offline -j2
cargo test --workspace --features shenko/compat-poppy --locked --offline --no-fail-fast -j2

python3 -m pip install --no-index --find-links <wheelhouse> --requirement scripts/requirements-ci.txt
(cd crates/shenko-ui/web && pnpm install --frozen-lockfile --offline)
(cd "$PROTO_UI_SOURCE" && pnpm install --frozen-lockfile --offline)

python3 scripts/ci/web_assets.py check
```

The replay is bound by `--locked` and `--frozen-lockfile`: a missing cache
entry or a lock mismatch is an error, not a reason to resolve a newer version.
If a lock must change, that change is a reviewed source change in the diff.
The same rule applies to the WASI SDK and model assets: the digest is checked
before use, and a different file is rejected rather than accepted
interactively.

`web_assets.py check` performs its own frozen installs and does not pass
`--offline`; with a fully warm store it does not need new packages, and a
missing entry makes the verifier fail. This page has not executed a fresh
network-disabled `web_assets.py check`; the command sequence above is the
documented replay path over the same frozen inputs, not a new verification
result.

Offline execution has been observed for selected Rust suites, not for the full
workspace or this verifier. [Agent node selection](agent-node-selection.md)
records `cargo test --locked --offline` runs with an empty `HOME`, an independent
Cargo/target/temp directory, the original lock unchanged, all 398 checked
registry archives matching their checksums, and inherited network-socket
denial. That is evidence about those selected suites and that prepared cache.
It is not a full offline CI replay and does not establish a product behavior.

CI still needs the network for its first fetch. The `Swatinem/rust-cache` and
`actions/cache` steps are performance caches keyed by the toolchain and
`Cargo.lock`; a cache hit is not provenance and cannot admit a new input. The
verifier re-reads the committed manifest, locks, and bytes on every run.

## The frontend digest manifest

The browser assets are committed minified bytes and are embedded by Rust with
`include_str!`, so there is no Node server at runtime. The expected bytes are
recorded in [`crates/shenko-ui/src/static/build-manifest.json`](../../crates/shenko-ui/src/static/build-manifest.json),
which is itself embedded and served at `/assets/build-manifest.json` by
[`crates/shenko-ui/src/routes.rs`](../../crates/shenko-ui/src/routes.rs). The
manifest is schema version 1 and holds:

- an `artifacts` map from every emitted path (including JavaScript chunks and
  license texts) to its byte count and SHA-256; membership is exact, so a
  stale extra file fails as well as a changed file;
- the pinned `upstream` block: repository, commit, previous reviewed commit,
  lock digest, and the real public-closure build receipt;
- the `compiler` facts: package and entry digests, compiler source-tree
  digest, bundle digest/bytes, dependency resolutions, and per-Prototype
  entry, generated-source, and provenance digests plus compiler IR/backend;
- a `toolchain` record of the actual Node, pnpm, esbuild, React, and React DOM
  versions used for the build.

[`crates/shenko-ui/web/build.mjs`](../../crates/shenko-ui/web/build.mjs) writes
the manifest from the actual build. It refuses a Proto-UI checkout whose commit
or `pnpm-lock.yaml` digest differs from `upstream-pin.json`. The Rust side
embeds the same file; the web build does not hand-edit the embedded assets.

[`scripts/ci/web_assets.py`](../../scripts/ci/web_assets.py) is the independent
verifier and has two phases:

1. `prepare` validates the committed static directory against
   `build-manifest.json` (exact membership and every recorded digest/byte
   count), cross-checks `upstream-pin.json` against the manifest, requires a
   full 40-hex commit and a retained build receipt, copies the committed static
   bytes and a clean source snapshot, fetches the pinned Proto-UI commit,
   checks its lock digest, and records the admitted inputs.
2. `check` refuses a reused `node_modules`, `generated`, or rebuilt-output tree;
   requires the actual Node and pnpm versions to equal the pins; rechecks that
   the admitted inputs did not change before and after installation; installs
   both trees with `--frozen-lockfile`; requires the upstream lock to stay
   unchanged; runs the upstream public-closure plan and build and matches the
   receipt to the plan; runs `pnpm run check`, `pnpm run test`, and
   `pnpm run build`; validates the rebuilt manifest and every compiler,
   dependency-resolution, Prototype, and provenance digest from the rebuilt
   bytes; then compares the stable manifest fields and the payload bytes.

Only run-local fields are excluded from the stable comparison: `built_at`,
`web_root`, `upstream.source_checkout`, and the build-receipt path and digest.
No payload, compiler, generated-source, or Prototype digest is excluded. The
verifier writes `comparison.json` (pin, Node/pnpm, both manifest digests, both
run records, the public plan, rebuilt payloads, verified files, and the full
error list) and exits nonzero on any difference. It never rewrites the expected
digests.

## CI verification

[`.github/workflows/ci.yml`](../../.github/workflows/ci.yml) declares six jobs,
and those literal job names are the required-check contract. The GitHub
ruleset that requires them is repository configuration, not a tracked file.
Each job runs on every pull request, merge group, and push to `main`; changed
paths, draft state, and local evidence do not bypass a job.

| Check | Main command | What it proves here | Retained evidence |
| --- | --- | --- | --- |
| `rust` | `cargo check --workspace --all-targets --features shenko/compat-poppy --locked -j2`, then `cargo test --workspace --features shenko/compat-poppy --locked --no-fail-fast -j2` | The workspace compiles and its tests/doctests run under Rust 1.98.1; the sha-pinned WASI SDK builds the SSH WASI guest; the optional DSH adapter is installed from its lock and tested | `rust-evidence`, `dsh-native-evidence` |
| `clippy-ratchet` | [`scripts/ci/base_ref.py`](../../scripts/ci/base_ref.py), then [`scripts/ci/clippy_ratchet.py`](../../scripts/ci/clippy_ratchet.py) `--base <independent SHA>` | Real production and all-target diagnostics are compared with the immutable PR/merge/push baseline; new warnings, unwrap/expect growth, suppressions, ignored tests, or lint-policy changes fail. The baseline must be a full nonzero ancestor SHA, not the candidate itself | `clippy-ratchet-evidence` |
| `docs` | `python3 scripts/check_docs.py` and `python3 scripts/observe_longrun_test.py` | Current documents, anchors, stable identities, links, and archive hashes are intact; the C19 recorder integrity and real-process concurrency cases run | job log |
| `docs-migration` | `python3 scripts/check_docs.py --verify-migration` | Original migrated contract blocks remain conserved; this is a separate gate from current references | job log |
| `kernel-boundary` | [`scripts/ci/base_ref.py`](../../scripts/ci/base_ref.py), then [`scripts/check_kernel_boundary.py`](../../scripts/check_kernel_boundary.py) `--baseline-ref <independent SHA>` | The workspace dependency direction is checked as structure only, using `cargo metadata --offline --locked --no-deps`; it does not build the product | `kernel-boundary-evidence` |
| `web` | [`scripts/ci/web_assets.py`](../../scripts/ci/web_assets.py) `prepare`, then `check` | The committed browser assets are independently rebuilt from the pinned Proto-UI source and compared to the manifest digests and payload bytes; the web typecheck, tests, and DSH bridge run | `web-rebuild-evidence` |

The pinned baseline for both ratchets comes from the GitHub event: the pull
request's base SHA, the merge group's base SHA, the push's `before`, or an
explicit `workflow_dispatch` input. `base_ref.py` rejects a missing, zero, or
head-equal candidate-supplied SHA and requires the resolved baseline to be an
ancestor of the candidate.

[`.github/workflows/nightly.yml`](../../.github/workflows/nightly.yml) adds the
longer work that is not a per-PR gate: the all-features workspace suite, the
ignored obligation-partition proof, the native SSH helper recovery/refusal
proofs, and the complete pinned Proto-UI compiler suite. The opt-in
[real single-node journey](../../.github/workflows/real-single-node.yml)
(`workflow_dispatch` or the `run-real-model` label) pins the Qwen3-0.6B model
`ac2d97712095a558e31573f62f466a3f9d93990898b0ec79d7c974c1780d524a` and the
llama.cpp `b12359` runtime archive
`0bc1f091e8916102b6d8f2cf944233a620e90a0f856c72d0ce41150c65c2e72e`, checks both
digests before use, and records the runtime version/help. The model server is
started with llama.cpp's `--offline` flag after that; this is not a
network-namespace proof.

A retained run is available in
[`docs/evidence/ci-3e62c0e5-20261116`](../evidence/ci-3e62c0e5-20261116/README.md):
that historical candidate's `rust` job compiled all targets and passed the
workspace tests, `docs`, `docs-migration`, `kernel-boundary`, and `web`
succeeded, and `clippy-ratchet` failed with retained comparison errors. The
archive keeps the original logs, job bodies, and artifact hashes. It is
evidence about that candidate and run, not about current `main`, and a green
`web` job still proves byte agreement rather than product behavior.

## What is still not pinned

- Container images are referenced by version tag, not by image digest, and
  GitHub Actions are referenced by major version, not commit SHA. The compiler
  version is pinned; the exact image bytes are not.
- `apt-get install` packages in the jobs (`cmake`, `clang`, `libclang-dev`,
  `pkg-config`, `libssl-dev`, `openssh-server`, and others) are not version- or
  digest-pinned.
- `scripts/requirements-ci.txt` pins versions but does not use
  `--require-hashes`.
- Dependency archives are not vendored in Git, so a disconnected clean machine
  needs the prepared caches described above; the repository does not claim a
  self-contained offline checkout.
- T4's published-artifact requirement is still open. The compiler and
  Prototype packages are consumed from a pinned source checkout and built as a
  public closure; they are not yet fetched at the pinned version as published
  packages and verified in a clean consumer build.
- A SHA-256 match proves integrity against the recorded value, not who produced
  the artifact or that the product behavior is correct.

## Verifying this page

The same documentation gate as the `docs` job:

```sh
python3 -m venv .ci/python
.ci/python/bin/python -m pip install --requirement scripts/requirements-ci.txt
.ci/python/bin/python scripts/check_docs.py
.ci/python/bin/python scripts/check_docs.py --verify-migration
```

The first command checks current links, anchors, stable identities, and archive
hashes. The second separately checks conservation of the original migrated
contract blocks. Neither imports a compiler, builds Shenko, runs a model, or
grants product acceptance.
