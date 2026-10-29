# Acquiring inputs for the toybox→WASI port

Everything below is pinned; verify every checksum before building. The port
downloads nothing itself — all inputs are local artifacts you place here.

## 1. Upstream source

- **File:** `toybox-0.8.15.tar.gz`
- **URL:** https://landley.net/toybox/downloads/toybox-0.8.15.tar.gz
  (mirror: https://github.com/landley/toybox/releases/tag/0.8.15)
- **SHA-256:** `52452b68a6cf50f9a9728b0f844b03c4dd1f70463675b202e6cf0475bda3934f`

Keep this tarball on persistent storage. `build.sh` checks the pinned hash
before claiming a new run directory; it never builds in an existing tree:

```sh
sha256sum toybox-0.8.15.tar.gz
```

## 2. Private patched vendor tree

`build.sh` unpacks the pinned archive into `<run>/vendor/` and applies the
patches with `--fuzz=0`; the patch logs are retained in `<run>/logs/`.
Do not patch or build in an existing upstream or project source tree.
Upstream scripts normally delete generated files and their own output while
building. Patch `0008-build-retain-generated.patch` changes each cleanup in
single.sh/make.sh/genconfig.sh and the Makefile to call `retain-generated.sh`:
existing targets are **moved**, with their original paths recorded, into
`<run>/retired/item.*/payload`. `retired/INDEX.tsv` records moved and absent
targets. Failure to archive propagates to the original failure boundary;
there is no suppressed `rm`, successful no-op cleanup, or deletion fallback.
The config-alias branch copies into retained run-owned scratch instead of
moving the input and deleting it on exit.

`GENDIR`, `UNSTRIPPED`, config paths and `TMPDIR` are confined to the run;
retirement rejects paths or ancestors outside its vendor/applets/scratch
trees. The product miniconfig is copied, never moved or offered as the
generated-config output. Builds enforce `CPUS=1` and `MAKEFLAGS=-j1`.
This retention patch/helper has not been executed or build-verified by the
worker; guarded parent verification must check the real archive receipts.

## 3. WASI SDK

- **File:** `wasi-sdk-34.0-x86_64-linux.tar.gz`
- **URL:** https://github.com/WebAssembly/wasi-sdk/releases/tag/wasi-sdk-34
- **Release asset:** id `528366474`, size `192383077` bytes
- **SHA-256 (release-asset metadata digest):**
  `b761e3a0721dbae9c09a0059e5fdb2bf917d1b4a8a7b430fb3b5aafb0984b2c4`

The release publishes no `SHA256SUMS` file; verify the downloaded tarball
byte count and `sha256sum` against the asset metadata above (bound to tag
`wasi-sdk-34`), not a signed manifest.

Pass the extracted SDK directory, pinned source archive and a fresh
persistent run path (its parent must exist; the run itself must not):

```sh
tar xf wasi-sdk-34.0-x86_64-linux.tar.gz
./build.sh /path/to/wasi-sdk-34.0-x86_64-linux /path/to/toybox-0.8.15.tar.gz \
  /tank/shared/projects/shenko/build/c06-toybox-wasi-run-$(date +%Y%m%d-%H%M%S)
```

The SDK must contain `bin/clang` and `share/wasi-sysroot`; `build.sh`
compiles a retained probe (`<run>/probe.wasm`) first and exits clearly if
the SDK is wrong. No cleanup trap deletes the probe or any earlier run.

## 4. Host build tools

Toybox generates its config/flags headers with a *host* compile of
`kconfig/` and `scripts/` helpers. Requires: `cc` or `gcc`, `make`, `bash`,
`sed`, `egrep`, `xargs`, `tr`. No network access is used.

## 5. Output

`build.sh` writes `<run>/applets/<applet>` — one standalone WASI preview1
module per enabled applet — plus `probe.c`/`probe.wasm` and
`BUILD-RECEIPT.txt` (sdk path, source archive/hash, per-applet sha256). Each is
installable with:

```sh
shenko tool install --name toybox-<applet> --version 0.8.15 \
  --abi wasi_preview1 --file <run>/applets/<applet>
```

Run `SMOKE.md`'s checks before publishing artifacts.
