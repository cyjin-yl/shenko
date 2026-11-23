# Release build for source c931526c

The actual devcontainer release build for
`c931526c8ae8cd16f3e0cf56d733f1e4275d5bb0` completed successfully. Its original
manifest records a **90,370,560-byte** binary with SHA256
`1ad2899f2c7dff3c0cd5123a16d3dfbb893a9cc8ba85996d5f16a8023c0eeba1`.
The original [manifest.json](manifest.json) retains the binary path, source,
profile, command and timestamps. This receipt copies the completed build
records; it does not execute a new build, test or model request.

## Actual execution and retained evidence

| Field | Recorded value |
| --- | --- |
| Source | `c931526c8ae8cd16f3e0cf56d733f1e4275d5bb0` |
| Actual command | `cargo build --offline --locked --release -p shenko -j4` |
| Build start / finish | 2026-11-22 00:46:07 / 00:53:22 UTC |
| Cargo reported duration | 7 m 15 s |
| Build whole-second timestamp interval | 435 s |
| Host start / finish | 2026-11-22 00:46:05 / 00:53:25 UTC |
| Host whole-second timestamp interval | 440 s |
| Remote tool runtime | 440.00 s |
| Build / host exit | 0 / 0 |
| Devcontainer image | `sha256:28df77cc6beee4f88ee1f57922190dcb02414d78d284e8b0f91859e18728e565` |
| Rust / Cargo / Node | 1.98.1 / 1.98.1 / 24.22.1 |

The build, host and tool timings retain their different scopes and precision.
The [original build log](build.log.gz) contains the successful Cargo completion
and its warnings. No claim is made that those warnings were pre-existing.
[The tool completion](collection/build-completion.tool.json) records the
verified container image, original preflight, binary manifest and natural host
exit 0. Both actual runner scripts and all original small release files are
retained. The collector did not copy or rehash the large release binary while
the separate model observation was running; its identity above is the original
build manifest's recorded hash.

## Reused scoped evidence

The actual [preflight.json](preflight.json) accepted the 10 passed tests across
3 targets at `95cdc272866a5f31cedb790d263761523e9b0e2c`: the frozen-membership
fixture, the original-E1/single-effect native chain and 8 archive tests. Their
original source-bound receipts are preserved by evidence commit
`890ff73974899c9cb0d89871b57d1a797bb45b6f` in
`predecessor-membership-d16adbf8/`. They were **reused**, not executed again in
this release runner.

The original preflight verified that the fixed source is an ancestor of this
candidate, that all intervening changed paths are under `docs/` or `scripts/`,
and that the three original scoped logs still match their recorded lengths and
hashes. It also verified all 12 checked-in static assets against the actual
manifest. Local receipt verification independently compared the complete Git
changed-path list, the scoped summary hash and all 12 static payloads to those
original preflight values. The runtime Rust/UI source is unchanged from the
scoped candidate; the repository as a whole includes the listed documentation
and observer-script changes.

This build receipt does not establish a real-model journey result, and it does
not regrade the original d16 model batch's 0/3. A subsequent observation has its
own binary, observer and result identities. [summary.json](summary.json)
explicitly records zero new test executions and leaves model success
unassessed.

## Byte preservation

[retention-index.json](retention-index.json) maps 14 original files to their
remote sources, raw/stored lengths and SHA256 values. The build log is stored
as gzip with mtime 0; its decoded bytes match the original. Source file size,
mtime and inode remained stable across each copy. The exact collector source
is separately compressed in `collection/collector.py.gz`; it only copied
closed build records and did not open any model installation or binary.

The transfer archive was 13,980 bytes, SHA256
`63582faf26e8ce977982688d6a5c0e0857faa0fa5144c299767796d937ef85e1`.
[file-manifest.json](file-manifest.json) covers every file except itself and
distinguishes original files from collection records and derived descriptions.
The large binary, Cargo target/dependency files and the live model batch were
excluded from this collection.
